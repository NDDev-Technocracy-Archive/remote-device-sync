//! H.264 codec over OpenH264: Annex-B, constrained baseline, no B-frames.
//!
//! Software encode is the portability floor; hardware encoders plug in
//! behind the same `Encoder`/`Decoder` traits later.

use bytes::Bytes;
use openh264::decoder::{Decoder as OhDecoder, DecoderConfig};
use openh264::encoder::{
    BitRate, EncodedBitStream, Encoder as OhEncoder, EncoderConfig, FrameRate, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::{BGRA8Source, RGB8Source, RGBSource, YUVBuffer, YUVSource};
use openh264::{Error as OhError, OpenH264API};
use rds_core::Codec;

use crate::{Decoder, DesktopError, EncodedFrame, Encoder, RawFrame};

/// Real-time OpenH264 encoder feeding `EncodedFrame`s.
pub struct H264Encoder {
    inner: OhEncoder,
    /// Rate the live encoder is configured for.
    bitrate: u64,
    /// Requested rate waiting to be applied — `openh264` has no live
    /// bitrate setter, so a change rebuilds the encoder lazily at the
    /// next `encode` (the rebuilt encoder's first frame is an IDR with
    /// fresh SPS/PPS, which is exactly the resync a rate shift wants).
    pending_bitrate: Option<u32>,
    fps: f32,
    want_idr: bool,
}

impl H264Encoder {
    pub fn new(bitrate_bps: u64, fps: f32) -> Result<Self, DesktopError> {
        let inner = Self::build(bitrate_bps as u32, fps)?;
        Ok(Self {
            inner,
            bitrate: bitrate_bps,
            pending_bitrate: None,
            fps,
            want_idr: true,
        })
    }

    fn build(bitrate_bps: u32, fps: f32) -> Result<OhEncoder, DesktopError> {
        let config = EncoderConfig::new()
            // Screen content: the desktop is text and sharp edges, not
            // camera footage — this tunes QP/mode decisions for it.
            .usage_type(UsageType::ScreenContentRealTime)
            // Unsupported for screen content — OpenH264 disables them
            // with a warning; set explicitly instead.
            .adaptive_quantization(false)
            .background_detection(false)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(fps))
            // ~8s at 30fps: a bound on how long a client that missed
            // every resync hint stays undecodable.
            .intra_frame_period(IntraFramePeriod::from_num_frames(240))
            .skip_frames(true);
        let api = OpenH264API::from_source();
        OhEncoder::with_api_config(api, config).map_err(|e| DesktopError::Encode(e.to_string()))
    }
}

/// Smallest relative bitrate change worth an encoder rebuild — smaller
/// steps are carried by the writer's token-bucket pacing alone, so the
/// controller's gentle recovery probes don't keep resetting rate
/// control state.
const BITRATE_REBUILD_MIN_PCT: u64 = 15;

impl Encoder for H264Encoder {
    fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, DesktopError> {
        if let Some(bps) = self.pending_bitrate.take() {
            match Self::build(bps, self.fps) {
                Ok(inner) => {
                    self.inner = inner;
                    self.bitrate = u64::from(bps);
                    self.want_idr = true;
                }
                Err(e) => tracing::warn!("encoder rebuild failed, keeping old rate: {e}"),
            }
        }
        if self.want_idr {
            self.inner.force_intra_frame();
            self.want_idr = false;
        }
        let yuv = bgra_to_i420(frame);
        let stream = self
            .inner
            .encode(&yuv)
            .map_err(|e| DesktopError::Encode(e.to_string()))?;
        // The flag the writer's collapse trusts must be the truth on
        // the wire, not a schedule assumption: OpenH264 decides when
        // forced and periodic IDRs actually land, so read the NALs.
        let keyframe = bitstream_has_idr(&stream);
        Ok(EncodedFrame {
            codec: Codec::H264,
            data: Bytes::from(stream.to_vec()),
            keyframe,
        })
    }

    fn request_idr(&mut self) {
        self.want_idr = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        let bps = u64::from(bps);
        if bps == 0 || bps == self.bitrate {
            return;
        }
        if bps.abs_diff(self.bitrate) * 100 < self.bitrate * BITRATE_REBUILD_MIN_PCT {
            return;
        }
        self.pending_bitrate = Some(bps as u32);
    }
}

/// Whether the emitted bitstream carries an IDR picture — the flag the
/// writer's collapse trusts. Read off the NALs rather than assumed:
/// OpenH264 decides when forced and periodic IDRs actually land.
/// NAL units arrive Annex-B wrapped: a 3- or 4-byte start code, then
/// the header byte whose low five bits are the unit type (5 = IDR).
fn bitstream_has_idr(stream: &EncodedBitStream<'_>) -> bool {
    for l in 0..stream.num_layers() {
        let Some(layer) = stream.layer(l) else {
            continue;
        };
        for n in 0..layer.nal_count() {
            let Some(nal) = layer.nal_unit(n) else {
                continue;
            };
            // First non-zero byte is the start code's trailing 0x01;
            // the byte right after it is the NAL header.
            let Some(hdr) = nal
                .iter()
                .position(|&b| b != 0)
                .and_then(|i| nal.get(i + 1))
            else {
                continue;
            };
            if hdr & 0x1f == 5 {
                return true;
            }
        }
    }
    false
}

/// OpenH264 decoder producing RGB8 frames.
pub struct H264Decoder {
    inner: OhDecoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, DesktopError> {
        let api = OpenH264API::from_source();
        let inner = OhDecoder::with_api_config(api, DecoderConfig::default())
            .map_err(|e| DesktopError::Decode(e.to_string()))?;
        Ok(Self { inner })
    }
}

impl Decoder for H264Decoder {
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<RawFrame>, DesktopError> {
        let Some(yuv) = self
            .inner
            .decode(&frame.data)
            .map_err(|e: OhError| DesktopError::Decode(e.to_string()))?
        else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let mut rgb = vec![0u8; w * h * 3];
        yuv.write_rgb8(&mut rgb);
        // Expand RGB to BGRA for a uniform RawFrame layout.
        let mut bgra = vec![0u8; w * h * 4];
        for (dst, src) in bgra
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(rgb.as_chunks::<3>().0)
        {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 0xFF;
        }
        Ok(Some(RawFrame {
            width: w as u32,
            height: h as u32,
            stride: w as u32 * 4,
            data: Bytes::from(bgra),
        }))
    }
}

/// BGRA8 → I420 (BT.601 studio swing) for OpenH264, via the encoder
/// crate's own converter — it dispatches to AVX2 at runtime on x86-64
/// (scalar elsewhere), honors arbitrary row strides, and box-averages
/// chroma. Stride-incompatible buffers take the scalar path.
pub fn bgra_to_i420(frame: &RawFrame) -> YUVBuffer {
    let stride = frame.stride as usize;
    if stride.is_multiple_of(4) && frame.data.len() >= stride * frame.height as usize {
        return YUVBuffer::from_bgra8_source(StridedBgra(frame));
    }
    bgra_to_i420_scalar(frame)
}

/// `RawFrame` as an `openh264` BGRA source, carrying its real stride so
/// padded capture buffers need no intermediate copy.
struct StridedBgra<'a>(&'a RawFrame);

impl RGBSource for StridedBgra<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.0.width as usize, self.0.height as usize)
    }

    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let o = y * self.0.stride as usize + x * 4;
        let px = &self.0.data[o..o + 4];
        (px[2] as f32, px[1] as f32, px[0] as f32)
    }
}

impl RGB8Source for StridedBgra<'_> {
    fn dimensions_padded(&self) -> (usize, usize) {
        (self.0.stride as usize / 4, self.0.height as usize)
    }

    fn rgb8_data(&self) -> &[u8] {
        &self.0.data
    }

    fn pixel_stride(&self) -> usize {
        4
    }

    fn rgb_channel_offsets(&self) -> (usize, usize, usize) {
        (2, 1, 0)
    }
}

impl BGRA8Source for StridedBgra<'_> {}

/// Scalar fallback for strides that are not whole pixels — also the
/// test reference for the SIMD path's output.
fn bgra_to_i420_scalar(frame: &RawFrame) -> YUVBuffer {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let stride = frame.stride as usize;
    let mut yuv = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv) = yuv.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
    let src = &frame.data;
    for row in 0..h {
        let srow = &src[row * stride..row * stride + w * 4];
        let yrow = &mut y_plane[row * w..row * w + w];
        for (x, px) in srow.as_chunks::<4>().0.iter().enumerate() {
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            yrow[x] = clamp8((66 * r + 129 * g + 25 * b + 128) / 256 + 16);
            if row % 2 == 0 && x % 2 == 0 {
                let i = (row / 2) * (w / 2) + x / 2;
                u_plane[i] = clamp8((-38 * r - 74 * g + 112 * b + 128) / 256 + 128);
                v_plane[i] = clamp8((112 * r - 94 * g - 18 * b + 128) / 256 + 128);
            }
        }
    }
    YUVBuffer::from_vec(yuv, w, h)
}

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> RawFrame {
        let (w, h) = (64u32, 64u32);
        RawFrame {
            width: w,
            height: h,
            stride: w * 4,
            data: Bytes::from(vec![0x5Au8; (w * h * 4) as usize]),
        }
    }

    /// The keyframe flag must describe the emitted bitstream: IDR on
    /// frame 0 (fresh encoder) and after `request_idr`, deltas between.
    #[test]
    fn keyframe_flag_matches_emitted_nals() {
        let mut enc = H264Encoder::new(1_000_000, 30.0).unwrap();
        assert!(enc.encode(&frame()).unwrap().keyframe, "first frame");
        for _ in 0..3 {
            assert!(
                !enc.encode(&frame()).unwrap().keyframe,
                "delta flagged as keyframe"
            );
        }
        enc.request_idr();
        assert!(
            enc.encode(&frame()).unwrap().keyframe,
            "forced IDR not flagged"
        );
        assert!(!enc.encode(&frame()).unwrap().keyframe);
    }

    /// A bitrate change beyond the deadband rebuilds the encoder — the
    /// next frame is an IDR carrying fresh SPS/PPS.
    #[test]
    fn bitrate_change_rebuilds_with_idr() {
        let mut enc = H264Encoder::new(1_000_000, 30.0).unwrap();
        enc.encode(&frame()).unwrap();
        // Below the deadband: no rebuild, no forced keyframe.
        enc.set_bitrate(1_100_000);
        assert!(!enc.encode(&frame()).unwrap().keyframe);
        // Past it: the rebuilt encoder's first frame is an IDR.
        enc.set_bitrate(2_000_000);
        assert!(enc.encode(&frame()).unwrap().keyframe);
    }

    /// The SIMD/dispatched converter must agree with the scalar
    /// reference: identical luma (same BT.601 coefficients), chroma
    /// within box-average-vs-nearest tolerance.
    #[test]
    fn simd_conversion_matches_scalar() {
        let (w, h) = (64u32, 64u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = (y * w as usize + x) * 4;
                if x < 32 {
                    // Solid red half: box-average == nearest-sample.
                    data[o..o + 4].copy_from_slice(&[0, 0, 255, 255]);
                } else {
                    data[o] = (x % 256) as u8;
                    data[o + 1] = ((y * 3) % 256) as u8;
                    data[o + 2] = ((x * 2) % 256) as u8;
                    data[o + 3] = 255;
                }
            }
        }
        let raw = RawFrame {
            width: w,
            height: h,
            stride: w * 4,
            data: Bytes::from(data),
        };
        let fast = bgra_to_i420(&raw);
        let slow = bgra_to_i420_scalar(&raw);
        assert_eq!(fast.dimensions(), slow.dimensions());
        for (a, b) in fast.y().iter().zip(slow.y()) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 2, "Y diverges");
        }
        // Solid half: chroma must match tightly.
        for row in 0..h as usize / 2 {
            for x in 0..16 {
                let i = row * (w as usize / 2) + x;
                assert!(
                    (i32::from(fast.u()[i]) - i32::from(slow.u()[i])).abs() <= 2,
                    "U diverges on solid region"
                );
                assert!(
                    (i32::from(fast.v()[i]) - i32::from(slow.v()[i])).abs() <= 2,
                    "V diverges on solid region"
                );
            }
        }
        // Gradient half: same colorspace, different chroma sampling —
        // bounded divergence only.
        for (a, b) in fast.u().iter().zip(slow.u()) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 32, "U diverges");
        }
    }

    /// Padded strides must be honored — padding bytes never leak into
    /// the planes (filled with a sentinel).
    #[test]
    fn strided_input_ignores_padding() {
        let (w, h, pad) = (64u32, 64u32, 32u32);
        let tight = frame();
        let stride = (w * 4 + pad) as usize;
        let mut data = vec![0u8; stride * h as usize];
        for row in 0..h as usize {
            data[row * stride..row * stride + (w * 4) as usize]
                .copy_from_slice(&tight.data[row * w as usize * 4..(row + 1) * w as usize * 4]);
            for b in &mut data[row * stride + (w * 4) as usize..(row + 1) * stride] {
                *b = 0xAA;
            }
        }
        let padded = RawFrame {
            width: w,
            height: h,
            stride: stride as u32,
            data: Bytes::from(data),
        };
        let a = bgra_to_i420(&padded);
        let b = bgra_to_i420(&tight);
        assert_eq!(a.y(), b.y());
        assert_eq!(a.u(), b.u());
        assert_eq!(a.v(), b.v());
    }
}
