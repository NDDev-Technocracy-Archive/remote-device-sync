//! Software encoding consumes the latest owned native image per monitor.

use crate::capture::latest::Slot;
use crate::{
    BgraFrame, DesktopError, Encoder, FrameProducer, H264Encoder, Produced, ProducerControls,
    SessionClock,
};
use rds_core::FrameHeader;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::OwnedSemaphorePermit;

pub(crate) struct Producer {
    _permit: OwnedSemaphorePermit,
    slot: Arc<Slot>,
    encoder: H264Encoder,
    extent: (u32, u32),
    height: Option<u32>,
    interval: Duration,
    due: Instant,
    work: Duration,
    generation: u64,
    skipped: bool,
}
impl Producer {
    pub fn new(
        permit: OwnedSemaphorePermit,
        slot: Arc<Slot>,
        interval: Duration,
        requested: Option<u32>,
        extent: Option<(u32, u32)>,
    ) -> Result<Self, DesktopError> {
        let height = match requested {
            Some(0) => None,
            Some(height) if (16..=4320).contains(&height) => Some(height),
            Some(_) => return Err(DesktopError::Capture("invalid video height".into())),
            None => match std::env::var("RDS_DESKTOP_OUTPUT_HEIGHT") {
                Ok(value) => Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|h| (16..=4320).contains(h))
                        .ok_or_else(|| {
                            DesktopError::Capture("invalid RDS_DESKTOP_OUTPUT_HEIGHT".into())
                        })?,
                ),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => {
                    return Err(DesktopError::Capture(
                        "invalid RDS_DESKTOP_OUTPUT_HEIGHT".into(),
                    ));
                }
            },
        };
        let extent =
            extent.ok_or_else(|| DesktopError::Capture("missing admitted native extent".into()))?;
        if slot.extent()? != Some(extent) || interval.is_zero() {
            return Err(DesktopError::Capture(
                "native display changed during admission".into(),
            ));
        }
        Ok(Self {
            _permit: permit,
            slot,
            encoder: H264Encoder::new(4_000_000, 1.0 / interval.as_secs_f32())?,
            extent,
            height,
            interval,
            due: Instant::now(),
            work: Duration::ZERO,
            generation: 0,
            skipped: false,
        })
    }
    fn next(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Result<Produced, DesktopError> {
        let slot = &self.slot;
        let idle_end = Instant::now() + Duration::from_secs(1);
        let mut image = slot.image(Duration::from_secs(1))?;
        let resumed = image.generation == self.generation;
        while image.generation == self.generation
            && !controls.idr.load(Ordering::Relaxed)
            && !controls.input_refresh_active(clock.now_ms())
            && Instant::now() < idle_end
        {
            std::thread::sleep(Duration::from_millis(10));
            image = slot.image(Duration::from_millis(10))?;
        }
        let interval = self.interval.max(self.work);
        if crate::session::advance_cadence(&mut self.due, interval, Instant::now(), resumed) {
            controls.deadline_misses.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(wait) = self.due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        // Refresh after cadence waiting; never encode an earlier retained image.
        image = slot.image(Duration::from_secs(1))?;
        let frame = &image.frame;
        if (frame.width, frame.height) != self.extent {
            return Err(DesktopError::Capture(
                "native display geometry changed".into(),
            ));
        }
        self.generation = image.generation;
        let capture_ts_ms = clock
            .now_ms()
            .saturating_sub(image.received.elapsed().as_millis().min(u64::MAX as u128) as u64);
        let started = Instant::now();
        if controls.idr.swap(false, Ordering::Relaxed) {
            self.encoder.request_idr();
        }
        self.encoder.set_bitrate(
            controls
                .bitrate
                .load(Ordering::Relaxed)
                .min(u64::from(u32::MAX)) as u32,
        );
        let raw = BgraFrame::from(frame);
        let scaled = self
            .height
            .filter(|h| *h < raw.height)
            .map(|h| crate::scaling::downscale(raw, h))
            .transpose()?;
        let raw = scaled.as_ref().map(BgraFrame::from).unwrap_or(raw);
        let encoded = self.encoder.encode_bgra(raw)?;
        self.work = started.elapsed();
        self.skipped = encoded.data.is_empty();
        Ok(Produced {
            header: FrameHeader {
                seq,
                capture_ts_ms,
                encode_done_ts_ms: clock.now_ms(),
                send_ts_ms: 0,
                keyframe: encoded.keyframe,
                codec: encoded.codec,
                width: raw.width,
                height: raw.height,
            },
            payload: encoded.data,
        })
    }
}
impl FrameProducer for Producer {
    fn resume_after_backpressure(&mut self) {
        crate::session::resume_cadence(
            &mut self.due,
            self.interval.max(self.work).min(Duration::from_millis(500)),
            Instant::now(),
        );
    }
    fn preserves_reference(&self) -> bool {
        self.skipped
    }
    fn produce(
        &mut self,
        seq: u64,
        controls: &ProducerControls,
        clock: &SessionClock,
    ) -> Option<Produced> {
        self.skipped = false;
        match self.next(seq, controls, clock) {
            Ok(frame) => Some(frame),
            Err(error) => {
                tracing::warn!(%error, "native capture/encode ended");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoder, EncodedFrame, H264Decoder, RawFrame};
    use tokio::sync::Semaphore;

    #[test]
    fn the_first_encoded_frame_contains_the_final_inactive_native_update() {
        let slot = Slot::new(0, Some((64, 64)));
        for value in [0, 255] {
            slot.publish(RawFrame {
                width: 64,
                height: 64,
                stride: 256,
                data: bytes::Bytes::from([value, value, value, 255].repeat(64 * 64)),
            });
        }
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let mut producer = Producer::new(
            permit,
            slot,
            Duration::from_millis(33),
            Some(0),
            Some((64, 64)),
        )
        .unwrap();
        let produced = producer
            .produce(
                0,
                &ProducerControls::new(4_000_000),
                &SessionClock::default(),
            )
            .unwrap();
        let decoded = H264Decoder::new()
            .unwrap()
            .decode(&EncodedFrame {
                codec: produced.header.codec,
                data: produced.payload,
                keyframe: produced.header.keyframe,
            })
            .unwrap()
            .unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 64));
        assert!(
            decoded
                .data
                .chunks_exact(4)
                .all(|pixel| pixel[..3].iter().all(|value| *value >= 240))
        );
    }
}
