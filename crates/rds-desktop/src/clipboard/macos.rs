//! Serialized AppKit pasteboard edge. Cocoa objects stay on their native owner;
//! only validated Rust values and selected-file capabilities leave it. General
//! and isolated probe boards share this implementation, without TCC mutation.
#![allow(unsafe_code)]
use std::sync::Mutex;

use bytes::Bytes;
use objc2::{
    ClassType, msg_send,
    rc::{Retained, autoreleasepool},
};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSString, NSUTF8StringEncoding};
use rds_core::clipboard::{Format, Representation};

use super::{
    image::Png,
    native::{Error, Policy, Publication, Snapshot, Text, Value},
};

#[cfg(feature = "host")]
mod owner;
#[cfg(feature = "host")]
pub(crate) use owner::Group;
#[cfg(all(feature = "host", feature = "viewer"))]
pub(crate) use owner::probe as owner_probe;

// NSPasteboard is not a concurrent mutable object. The asynchronous provider
// uses a serialized native owner; the existing viewer adapter is being moved
// onto that same provider before native clipboard mode is advertised.
static ACCESS: Mutex<()> = Mutex::new(());

pub(crate) struct Board {
    board: Retained<NSPasteboard>,
    published: Option<u64>,
    isolated: bool,
}

enum Raw {
    Text(String),
    Png(Bytes),
    Tiff(Bytes),
}

impl Board {
    pub(crate) fn general() -> Result<Self, Error> {
        // SAFETY: Objective-C owns and retains this AppKit factory result. An
        // unavailable login pasteboard may return nil despite the annotation;
        // Option preserves that failure instead of constructing a null object.
        let board: Option<Retained<NSPasteboard>> =
            unsafe { msg_send![NSPasteboard::class(), generalPasteboard] };
        Ok(Self {
            board: board.ok_or(Error::Unavailable)?,
            published: None,
            isolated: false,
        })
    }

    #[cfg(feature = "viewer")]
    fn isolated() -> Self {
        Self {
            board: NSPasteboard::pasteboardWithUniqueName(),
            published: None,
            isolated: true,
        }
    }

    pub(crate) fn generation(&self) -> Result<u64, Error> {
        let _access = ACCESS.lock().map_err(|_| Error::Unavailable)?;
        Ok(autoreleasepool(|_| self.board.changeCount() as u64))
    }

    pub(crate) fn snapshot(&self, policy: &Policy) -> Result<Snapshot, Error> {
        policy.validate()?;
        let _access = ACCESS.lock().map_err(|_| Error::Unavailable)?;
        autoreleasepool(|_| {
            let generation = self.board.changeCount() as u64;
            let mut representations = Vec::new();
            if let Some(types) = self.board.types() {
                // Native type metadata is bounded before it is enumerated.
                if types.count() > policy.native_types {
                    return Err(Error::Unsupported);
                }
                // SAFETY: these immutable framework-owned type constants stay
                // valid for the whole process; no native object crosses threads.
                unsafe {
                    for (format, present) in [
                        (
                            Format::TextUtf8,
                            types.containsObject(NSPasteboardTypeString),
                        ),
                        (
                            Format::ImagePng,
                            types.containsObject(NSPasteboardTypePNG)
                                || types.containsObject(NSPasteboardTypeTIFF),
                        ),
                    ] {
                        if present {
                            representations.push(Representation {
                                format,
                                bytes: None,
                            });
                        }
                    }
                }
            }
            if self.board.changeCount() as u64 != generation {
                return Err(Error::Changed);
            }
            Ok(Snapshot {
                generation,
                owned: self.published == Some(generation),
                representations,
            })
        })
    }

    pub(crate) fn read(
        &self,
        generation: u64,
        format: Format,
        policy: &Policy,
    ) -> Result<Value, Error> {
        policy.validate()?;
        let raw = {
            let _access = ACCESS.lock().map_err(|_| Error::Unavailable)?;
            autoreleasepool(|_| {
                if self.board.changeCount() as u64 != generation {
                    return Err(Error::Changed);
                }
                // SAFETY: immutable AppKit type constants; returned objects are
                // retained and copied with bounds while this native owner runs.
                let raw = unsafe {
                    match format {
                        Format::TextUtf8 => {
                            let text = self
                                .board
                                .stringForType(NSPasteboardTypeString)
                                .ok_or(Error::Unsupported)?;
                            if text.length() as u64 > policy.limits.text_bytes
                                || !text.canBeConvertedToEncoding(NSUTF8StringEncoding)
                                || text.lengthOfBytesUsingEncoding(NSUTF8StringEncoding) as u64
                                    > policy.limits.text_bytes
                            {
                                return Err(rds_core::clipboard::Error::TooLarge.into());
                            }
                            Raw::Text(text.to_string())
                        }
                        Format::ImagePng => {
                            let (data, png) =
                                if let Some(data) = self.board.dataForType(NSPasteboardTypePNG) {
                                    (data, true)
                                } else {
                                    (
                                        self.board
                                            .dataForType(NSPasteboardTypeTIFF)
                                            .ok_or(Error::Unsupported)?,
                                        false,
                                    )
                                };
                            if data.length() as u64 > policy.limits.image_bytes {
                                return Err(rds_core::clipboard::Error::TooLarge.into());
                            }
                            let bytes = Bytes::from(data.to_vec());
                            if png {
                                Raw::Png(bytes)
                            } else {
                                Raw::Tiff(bytes)
                            }
                        }
                        Format::Files => return Err(Error::Unsupported),
                    }
                };
                if self.board.changeCount() as u64 != generation {
                    return Err(Error::Changed);
                }
                Ok(raw)
            })?
        };
        // Parsing/conversion/file opens never retain the AppKit mutation lock.
        let value = match raw {
            Raw::Text(text) => Value::Text(Text::new(Bytes::from(text), &policy.limits)?),
            Raw::Png(bytes) => Value::Image(Png::new(bytes, &policy.limits)?),
            Raw::Tiff(bytes) => Value::Image(Png::from_tiff(&bytes, &policy.limits)?),
        };
        if self.generation()? != generation {
            return Err(Error::Changed);
        }
        Ok(value)
    }

    pub(crate) fn publish(
        &mut self,
        generation: u64,
        value: Publication,
        policy: &Policy,
    ) -> Result<Snapshot, Error> {
        policy.validate()?;
        // Reject unavailable file-promise publication before changing the board.
        // Its receive capability stays absent until the native provider lands.
        if matches!(value, Publication::Files(_)) {
            return Err(Error::Unsupported);
        }
        match &value {
            Publication::Text(text) => policy
                .limits
                .validate_value(Format::TextUtf8, text.encoded())?,
            Publication::Image(image) => policy
                .limits
                .validate_value(Format::ImagePng, image.encoded())?,
            Publication::Files(_) => return Err(Error::Unsupported),
        }
        {
            let _access = ACCESS.lock().map_err(|_| Error::Unavailable)?;
            autoreleasepool(|_| {
                if self.board.changeCount() as u64 != generation {
                    return Err(Error::Changed);
                }
                // SAFETY: immutable framework type constants. Every body was
                // validated before ownership changes; native copies are retained
                // through the synchronous publication on this serialized owner.
                let published = unsafe {
                    match value {
                        Publication::Text(text) => {
                            let native = NSString::from_str(text.as_str());
                            self.board.clearContents();
                            self.board
                                .setString_forType(&native, NSPasteboardTypeString)
                        }
                        Publication::Image(image) => {
                            let native = NSData::with_bytes(image.encoded());
                            self.board.clearContents();
                            self.board
                                .setData_forType(Some(&native), NSPasteboardTypePNG)
                        }
                        Publication::Files(_) => return Err(Error::Unsupported),
                    }
                };
                if !published {
                    return Err(Error::Publication);
                }
                self.published = Some(self.board.changeCount() as u64);
                Ok(())
            })?;
        }
        let snapshot = self.snapshot(policy)?;
        if !snapshot.owned {
            return Err(Error::Changed);
        }
        Ok(snapshot)
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        if self.isolated
            && let Ok(_access) = ACCESS.lock()
        {
            self.board.clearContents();
        }
    }
}

/// Native named-board probe; never replaces the user's general clipboard.
#[cfg(feature = "viewer")]
pub(crate) fn probe() -> Result<u32, Error> {
    let mut board = Board::isolated();
    let policy = Policy::default();
    for text in [
        String::new(),
        "Привет 🖥️\nHello".into(),
        "UTF-8 🖥️\n".repeat(8000),
    ] {
        let before = board.generation()?;
        let text = Text::new(Bytes::from(text), &policy.limits)?;
        let expected = text.encoded().clone();
        let snapshot = board.publish(before, Publication::Text(text), &policy)?;
        let Value::Text(read) = board.read(snapshot.generation, Format::TextUtf8, &policy)? else {
            return Err(Error::Unsupported);
        };
        if read.encoded() != &expected || !snapshot.owned || snapshot.generation == before {
            return Err(Error::Publication);
        }
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| Error::Unavailable)?;
        writer
            .write_image_data(&[255, 40, 20, 128])
            .map_err(|_| Error::Unavailable)?;
        writer.finish().map_err(|_| Error::Unavailable)?;
    }
    let image = Png::new(Bytes::from(encoded), &policy.limits)?;
    let expected = image.encoded().clone();
    let snapshot = board.publish(board.generation()?, Publication::Image(image), &policy)?;
    let Value::Image(read) = board.read(snapshot.generation, Format::ImagePng, &policy)? else {
        return Err(Error::Unsupported);
    };
    if read.encoded() != &expected {
        return Err(Error::Publication);
    }
    // Generation fencing must refuse a late result without replacing the image.
    if !matches!(
        board.publish(
            snapshot.generation.wrapping_sub(1),
            Publication::Text(Text::new(Bytes::from_static(b"stale"), &policy.limits)?),
            &policy
        ),
        Err(Error::Changed)
    ) {
        return Err(Error::Publication);
    }
    let strict = Policy {
        limits: rds_core::clipboard::Limits {
            text_bytes: 8,
            ..policy.limits.clone()
        },
        ..policy.clone()
    };
    let before = board.generation()?;
    let text = Text::new(
        Bytes::from_static(b"outside the stricter receiver policy"),
        &policy.limits,
    )?;
    if board
        .publish(before, Publication::Text(text), &strict)
        .is_ok()
        || board.generation()? != before
    {
        return Err(Error::Publication);
    }
    let oversized = "x".repeat(policy.limits.text_bytes as usize + 1);
    {
        let _access = ACCESS.lock().map_err(|_| Error::Unavailable)?;
        // SAFETY: isolated named-board fixture and immutable native type constant;
        // deliberately seed outside the Rust constructor to test native-read bounds.
        unsafe {
            board.board.clearContents();
            if !board
                .board
                .setString_forType(&NSString::from_str(&oversized), NSPasteboardTypeString)
            {
                return Err(Error::Publication);
            }
        }
    }
    if board
        .read(board.generation()?, Format::TextUtf8, &policy)
        .is_ok()
    {
        return Err(Error::Publication);
    }
    Ok(7)
}
