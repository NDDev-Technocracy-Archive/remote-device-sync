//! Checked mapped-memory floor for negotiated single-plane BGRA/BGRx video.

use crate::{DesktopError, RawFrame};
use bytes::Bytes;

pub(crate) fn copy_bgra(
    memory: &[u8],
    offset: u32,
    size: u32,
    stride: i32,
    extent: (u32, u32),
    opaque: bool,
) -> Result<RawFrame, DesktopError> {
    let (width, height) = extent;
    let invalid = || DesktopError::Capture("invalid native video buffer".into());
    let bytes = crate::frame_bytes(width as usize, height as usize).ok_or_else(invalid)?;
    let row = (width as usize).checked_mul(4).ok_or_else(invalid)?;
    let stride = usize::try_from(stride)
        .ok()
        .filter(|stride| *stride >= row)
        .ok_or_else(invalid)?;
    let needed = (height as usize - 1)
        .checked_mul(stride)
        .and_then(|n| n.checked_add(row))
        .ok_or_else(invalid)?;
    let end = (offset as usize)
        .checked_add(size as usize)
        .ok_or_else(invalid)?;
    let mapped = memory
        .get(offset as usize..end)
        .filter(|mapped| mapped.len() >= needed)
        .ok_or_else(invalid)?;
    let mut output = vec![0; bytes];
    for (y, destination) in output.chunks_exact_mut(row).enumerate() {
        destination.copy_from_slice(&mapped[y * stride..y * stride + row]);
        if opaque {
            for pixel in destination.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
    }
    Ok(RawFrame {
        width,
        height,
        stride: width * 4,
        data: Bytes::from(output),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_bgrx_rows_and_offset_preserve_colors_with_opaque_alpha() {
        let memory = [
            99, 99, 1, 2, 3, 0, 4, 5, 6, 0, 88, 88, 7, 8, 9, 0, 10, 11, 12, 0,
        ];
        let frame = copy_bgra(&memory, 2, 18, 10, (2, 2), true).unwrap();
        assert_eq!(
            &frame.data[..],
            &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        assert_eq!(frame.stride, 8);
    }

    #[test]
    fn malformed_native_chunks_fail_before_allocation_or_indexing() {
        for (offset, size, stride, extent) in [
            (0, 16, -8, (2, 2)),
            (0, 16, 4, (2, 2)),
            (0, 15, 8, (2, 2)),
            (9, 16, 8, (2, 2)),
            (0, 16, 8, (0, 2)),
            (0, 16, 8, (u32::MAX, 2)),
            (u32::MAX, u32::MAX, 8, (2, 2)),
        ] {
            assert!(copy_bgra(&[0; 16], offset, size, stride, extent, false).is_err());
        }
    }
}
