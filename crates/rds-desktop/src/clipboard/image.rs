//! Validated static PNG transport and bounded native TIFF conversion. No disk
//! cache, native image allocation or full PNG re-encoding on the common path.
use std::io::{Cursor, Write};

use bytes::Bytes;
use image::{ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat};
use rds_core::clipboard::{Format, Limits};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] rds_core::clipboard::Error),
    #[error("invalid or unsupported native clipboard image")]
    Image,
}

/// The only constructor validates the complete encoded body, not just IHDR.
/// Cloning retains the same immutable bytes; payloads have no Debug output.
#[derive(Clone)]
pub struct Png(Bytes);

impl Png {
    pub fn new(bytes: Bytes, limits: &Limits) -> Result<Self, Error> {
        limits.validate_value(Format::ImagePng, &bytes)?;
        let budget =
            usize::try_from(limits.image_pixels.saturating_mul(8)).map_err(|_| Error::Image)?;
        let mut decoder =
            png::Decoder::new_with_limits(Cursor::new(&bytes), png::Limits { bytes: budget });
        decoder.ignore_checksums(false);
        let mut reader = decoder.read_info().map_err(|_| Error::Image)?;
        if reader.info().animation_control.is_some() {
            return Err(Error::Image);
        }
        while reader.next_row().map_err(|_| Error::Image)?.is_some() {}
        reader.finish().map_err(|_| Error::Image)?;
        Ok(Self(bytes))
    }

    pub fn encoded(&self) -> &Bytes {
        &self.0
    }

    /// AppKit commonly provides TIFF for a copied raster. Read dimensions and
    /// enforce decoder allocation bounds before constructing native pixels.
    /// Conversion belongs on a bounded blocking image owner, never the UI loop.
    pub fn from_tiff(bytes: &[u8], limits: &Limits) -> Result<Self, Error> {
        limits.validate()?;
        if bytes.len() as u64 > limits.image_bytes {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), ImageFormat::Tiff);
        let mut allocation = image::Limits::default();
        allocation.max_alloc = Some(limits.image_pixels.saturating_mul(8));
        reader.limits(allocation);
        let decoder = reader.into_decoder().map_err(|_| Error::Image)?;
        let (width, height) = decoder.dimensions();
        if u64::from(width) * u64::from(height) > limits.image_pixels {
            return Err(rds_core::clipboard::Error::TooLarge.into());
        }
        let pixels = image::DynamicImage::from_decoder(decoder)
            .map_err(|_| Error::Image)?
            .into_rgba8();
        let mut output = BoundedOutput {
            bytes: Vec::new(),
            maximum: limits.image_bytes,
        };
        image::codecs::png::PngEncoder::new_with_quality(
            &mut output,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|_| Error::Image)?;
        Self::new(Bytes::from(output.bytes), limits)
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    maximum: u64,
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if (self.bytes.len() as u64).saturating_add(bytes.len() as u64) > self.maximum {
            return Err(std::io::Error::other(
                "encoded clipboard image exceeds its limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[255, 0, 0, 128, 0, 255, 0, 0])
            .unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn encoded_png_is_shared_and_corruption_or_allocation_bombs_are_rejected() {
        let bytes = Bytes::from(png());
        let image = Png::new(bytes.clone(), &Limits::default()).unwrap();
        assert_eq!(image.encoded().as_ptr(), bytes.as_ptr());
        for length in [0, 33, bytes.len() - 1] {
            assert!(Png::new(bytes.slice(..length), &Limits::default()).is_err());
        }
        let mut corrupt = bytes.to_vec();
        corrupt[32] ^= 1;
        assert!(Png::new(corrupt.into(), &Limits::default()).is_err());
        assert!(
            Png::new(
                bytes,
                &Limits {
                    image_pixels: 1,
                    ..Limits::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn native_tiff_conversion_preserves_pixels_and_alpha_without_a_file() {
        let source = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 128, 0, 255, 0, 0]).unwrap();
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, ImageFormat::Tiff).unwrap();
        let limits = Limits::default();
        let png = Png::from_tiff(encoded.get_ref(), &limits).unwrap();
        let received = image::load_from_memory_with_format(png.encoded(), ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(received.as_raw(), source.as_raw());
        assert!(
            Png::from_tiff(
                encoded.get_ref(),
                &Limits {
                    image_pixels: 1,
                    ..limits
                }
            )
            .is_err()
        );
    }
}
