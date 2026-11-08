//! Native clipboard representations and bounded, session-bound transfer data.
//! These types declare no OS access or authorization. The text-only desktop
//! wire remains unchanged until the new capabilities are explicitly negotiated.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const TEXT_BYTES: u64 = 1024 * 1024;
const IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const IMAGE_PIXELS: u64 = 4096 * 4096;
const SELECTION_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const FILES: u32 = 128;
const NAME_BYTES: usize = 1024;
const MANIFEST_BYTES: usize = 48 * 1024;
const CHUNK_BYTES: u32 = 256 * 1024;
const MAX_FORMATS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Format {
    TextUtf8,
    ImagePng,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShortcutModifier {
    Control,
    Command,
}

/// One selected policy. Peers use the smaller admitted limits; a capability
/// declaration alone never grants screen control, clipboard or filesystem I/O.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub text_bytes: u64,
    pub image_bytes: u64,
    pub image_pixels: u64,
    pub selection_bytes: u64,
    pub files: u32,
    pub chunk_bytes: u32,
    pub parallel_transfers: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            text_bytes: TEXT_BYTES,
            image_bytes: IMAGE_BYTES,
            image_pixels: IMAGE_PIXELS,
            selection_bytes: SELECTION_BYTES,
            files: FILES,
            chunk_bytes: CHUNK_BYTES,
            parallel_transfers: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid clipboard resource limits")]
    Limits,
    #[error("clipboard representation is not admitted")]
    UnsupportedFormat,
    #[error("clipboard value exceeds the admitted limit")]
    TooLarge,
    #[error("invalid clipboard transfer identity")]
    Identity,
    #[error("invalid clipboard representation list")]
    Formats,
    #[error("clipboard file name is not a portable leaf name")]
    FileName,
    #[error("clipboard file manifest contains duplicate names or identities")]
    DuplicateFile,
    #[error("clipboard text is not UTF-8")]
    Text,
    #[error("clipboard image has no bounded PNG dimensions")]
    Image,
}

impl Limits {
    pub fn validate(&self) -> Result<(), Error> {
        if self.text_bytes == 0
            || self.text_bytes > TEXT_BYTES
            || self.image_bytes == 0
            || self.image_bytes > IMAGE_BYTES
            || self.image_pixels == 0
            || self.image_pixels > IMAGE_PIXELS
            || self.selection_bytes == 0
            || self.selection_bytes > SELECTION_BYTES
            || self.files == 0
            || self.files > FILES
            || self.chunk_bytes == 0
            || self.chunk_bytes > CHUNK_BYTES
            || !(1..=2).contains(&self.parallel_transfers)
        {
            return Err(Error::Limits);
        }
        Ok(())
    }

    pub fn intersect(&self, peer: &Self) -> Result<Self, Error> {
        self.validate()?;
        peer.validate()?;
        Ok(Self {
            text_bytes: self.text_bytes.min(peer.text_bytes),
            image_bytes: self.image_bytes.min(peer.image_bytes),
            image_pixels: self.image_pixels.min(peer.image_pixels),
            selection_bytes: self.selection_bytes.min(peer.selection_bytes),
            files: self.files.min(peer.files),
            chunk_bytes: self.chunk_bytes.min(peer.chunk_bytes),
            parallel_transfers: self.parallel_transfers.min(peer.parallel_transfers),
        })
    }

    pub fn representation_bytes(&self, format: Format) -> u64 {
        match format {
            Format::TextUtf8 => self.text_bytes,
            Format::ImagePng => self.image_bytes,
            Format::Files => MANIFEST_BYTES as u64,
        }
    }

    pub fn validate_value(&self, format: Format, bytes: &[u8]) -> Result<(), Error> {
        self.validate()?;
        if bytes.len() as u64 > self.representation_bytes(format) {
            return Err(Error::TooLarge);
        }
        match format {
            Format::TextUtf8 => {
                std::str::from_utf8(bytes).map_err(|_| Error::Text)?;
            }
            Format::ImagePng => {
                // Only inspect the bounded header here. A native decoder must
                // additionally validate the complete PNG and its allocation.
                if bytes.len() < 33
                    || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
                    || bytes[8..12] != 13u32.to_be_bytes()
                    || &bytes[12..16] != b"IHDR"
                {
                    return Err(Error::Image);
                }
                let width = u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| Error::Image)?);
                let height =
                    u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| Error::Image)?);
                if width == 0 || height == 0 {
                    return Err(Error::Image);
                }
                if u64::from(width) * u64::from(height) > self.image_pixels {
                    return Err(Error::TooLarge);
                }
            }
            Format::Files => {} // The separately decoded FileSelection owns validation.
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub receive: Vec<Format>,
    pub offer: Vec<Format>,
    pub shortcut: ShortcutModifier,
    pub limits: Limits,
}

fn formats(values: impl Iterator<Item = Format>) -> Result<(), Error> {
    let mut unique = BTreeSet::new();
    for value in values {
        if unique.len() == MAX_FORMATS || !unique.insert(value) {
            return Err(Error::Formats);
        }
    }
    Ok(())
}

impl Capabilities {
    pub fn validate(&self) -> Result<(), Error> {
        self.limits.validate()?;
        formats(self.receive.iter().copied())?;
        formats(self.offer.iter().copied())
    }

    /// Return this endpoint's effective directions. The peer's receive list
    /// constrains our offers and its offer list constrains our native writes.
    pub fn negotiate(&self, peer: &Self) -> Result<Self, Error> {
        self.validate()?;
        peer.validate()?;
        Ok(Self {
            receive: self
                .receive
                .iter()
                .copied()
                .filter(|f| peer.offer.contains(f))
                .collect(),
            offer: self
                .offer
                .iter()
                .copied()
                .filter(|f| peer.receive.contains(f))
                .collect(),
            shortcut: self.shortcut,
            limits: self.limits.intersect(&peer.limits)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Representation {
    pub format: Format,
    /// Native providers may know types without reading the body. The receiver
    /// still bounds the actual stream header and every allocation.
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub id: u64,
    pub generation: u64,
    pub representations: Vec<Representation>,
}

impl Offer {
    pub fn validate(&self, admitted: &[Format], limits: &Limits) -> Result<(), Error> {
        limits.validate()?;
        if self.id == 0 {
            return Err(Error::Identity);
        }
        if self.representations.is_empty() {
            return Err(Error::Formats);
        }
        formats(self.representations.iter().map(|r| r.format))?;
        for representation in &self.representations {
            if !admitted.contains(&representation.format) {
                return Err(Error::UnsupportedFormat);
            }
            if representation
                .bytes
                .is_some_and(|n| n > limits.representation_bytes(representation.format))
            {
                return Err(Error::TooLarge);
            }
        }
        Ok(())
    }
}

/// Source paths never cross the wire. The native source binds these opaque IDs
/// to the exact selected objects and keeps that authority through I/O drain.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: [u8; 16],
    pub name: String,
    pub bytes: u64,
}

impl std::fmt::Debug for FileEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEntry")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSelection {
    pub entries: Vec<FileEntry>,
}

impl FileSelection {
    pub fn validate(&self, limits: &Limits) -> Result<u64, Error> {
        limits.validate()?;
        if self.entries.is_empty() || self.entries.len() > limits.files as usize {
            return Err(Error::TooLarge);
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut total = 0u64;
        let mut name_bytes = 0usize;
        for entry in &self.entries {
            if entry.id == [0; 16] {
                return Err(Error::Identity);
            }
            if entry.name.is_empty()
                || entry.name == "."
                || entry.name == ".."
                || entry.name.len() > NAME_BYTES
                || entry.name.contains('/')
                || entry.name.chars().any(char::is_control)
            {
                return Err(Error::FileName);
            }
            if !ids.insert(entry.id) || !names.insert(&entry.name) {
                return Err(Error::DuplicateFile);
            }
            name_bytes = name_bytes
                .checked_add(entry.name.len())
                .ok_or(Error::TooLarge)?;
            total = total.checked_add(entry.bytes).ok_or(Error::TooLarge)?;
            if name_bytes > MANIFEST_BYTES || total > limits.selection_bytes {
                return Err(Error::TooLarge);
            }
        }
        Ok(total)
    }
}

/// A transfer is pinned to an existing desktop, its exact offer and one request.
/// The owner additionally binds this to its authenticated connection and grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferId {
    pub desktop: [u8; 16],
    pub offer: u64,
    pub request: u64,
}

impl TransferId {
    pub fn validate(&self) -> Result<(), Error> {
        if self.desktop == [0; 16] || self.offer == 0 || self.request == 0 {
            Err(Error::Identity)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadKind {
    Representation(Format),
    File([u8; 16]),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferHeader {
    pub id: TransferId,
    pub kind: PayloadKind,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: TransferId,
    pub kind: PayloadKind,
    pub bytes: Option<u64>,
}

impl Request {
    pub fn validate(&self, limits: &Limits) -> Result<(), Error> {
        limits.validate()?;
        self.id.validate()?;
        let maximum = match self.kind {
            PayloadKind::Representation(format) => limits.representation_bytes(format),
            PayloadKind::File(id) if id != [0; 16] && self.bytes.is_some() => {
                limits.selection_bytes
            }
            PayloadKind::File(_) => return Err(Error::Identity),
        };
        if self.bytes.is_some_and(|bytes| bytes > maximum) {
            return Err(Error::TooLarge);
        }
        Ok(())
    }
}

impl TransferHeader {
    pub fn request(&self) -> Request {
        Request {
            id: self.id,
            kind: self.kind,
            bytes: Some(self.bytes),
        }
    }

    /// Admission binds both the session/request and the exact requested object.
    /// File lengths come from the already validated selection, not this header.
    pub fn validate(&self, expected: &Request, limits: &Limits) -> Result<(), Error> {
        expected.validate(limits)?;
        self.request().validate(limits)?;
        if self.id != expected.id
            || self.kind != expected.kind
            || expected.bytes.is_some_and(|bytes| bytes != self.bytes)
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub id: TransferId,
    pub bytes: u64,
    pub digest: [u8; 32],
    /// Complete verified receipt differs from native publication. Neither alone
    /// proves a destination application consumed the data.
    pub published: bool,
}

impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receipt")
            .field("id", &self.id)
            .field("bytes", &self.bytes)
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    Unavailable,
    Unsupported,
    Expired,
    InvalidRequest,
    TooLarge,
    Canceled,
    Integrity,
    NativePublication,
    Io,
}

/// Symmetric metadata only. Requested bodies use a separate low-priority
/// stream, never the desktop input/event lane. Capability negotiation must
/// precede every message; unknown support is not successful native admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    Offer(Offer),
    Withdraw { offer: u64 },
    Request(Request),
    Receipt(Receipt),
    Cancel { id: TransferId },
    Failed { id: TransferId, code: Failure },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_bounded_and_never_accepts_a_source_path_or_duplicate_target() {
        let limits = Limits::default();
        let item = FileEntry {
            id: [1; 16],
            name: "Привет ✓.txt".into(),
            bytes: 7,
        };
        let mut selection = FileSelection {
            entries: vec![item.clone()],
        };
        assert_eq!(selection.validate(&limits), Ok(7));
        assert!(!format!("{selection:?}").contains(&item.name));
        for name in ["../escape", "/absolute", ".", "..", "a\0b"] {
            selection.entries[0].name = name.into();
            assert_eq!(selection.validate(&limits), Err(Error::FileName));
        }
        selection.entries = vec![
            item.clone(),
            FileEntry {
                id: [2; 16],
                ..item.clone()
            },
        ];
        assert_eq!(selection.validate(&limits), Err(Error::DuplicateFile));
        selection.entries = vec![FileEntry {
            bytes: u64::MAX,
            ..item
        }];
        assert_eq!(selection.validate(&limits), Err(Error::TooLarge));
    }

    #[test]
    fn offers_respect_each_direction_without_guessing_unknown_payload_size() {
        let limits = Limits::default();
        let mut offer = Offer {
            id: 1,
            generation: 0,
            representations: vec![Representation {
                format: Format::TextUtf8,
                bytes: None,
            }],
        };
        assert_eq!(offer.validate(&[Format::TextUtf8], &limits), Ok(()));
        assert_eq!(offer.validate(&[], &limits), Err(Error::UnsupportedFormat));
        offer.representations.push(offer.representations[0]);
        assert_eq!(
            offer.validate(&[Format::TextUtf8], &limits),
            Err(Error::Formats)
        );
        offer.representations.pop();
        offer.representations[0].bytes = Some(limits.text_bytes + 1);
        assert_eq!(
            offer.validate(&[Format::TextUtf8], &limits),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn negotiation_is_directional_and_preserves_the_native_shortcut() {
        let a = Capabilities {
            receive: vec![Format::TextUtf8, Format::ImagePng],
            offer: vec![Format::Files, Format::TextUtf8],
            shortcut: ShortcutModifier::Command,
            limits: Limits::default(),
        };
        let b = Capabilities {
            receive: vec![Format::Files],
            offer: vec![Format::TextUtf8, Format::Files],
            shortcut: ShortcutModifier::Control,
            limits: Limits {
                text_bytes: 16,
                ..Limits::default()
            },
        };
        let negotiated = a.negotiate(&b).unwrap();
        assert_eq!(negotiated.receive, [Format::TextUtf8]);
        assert_eq!(negotiated.offer, [Format::Files]);
        assert_eq!(negotiated.shortcut, ShortcutModifier::Command);
        assert_eq!(negotiated.limits.text_bytes, 16);
    }

    #[test]
    fn text_and_image_headers_are_validated_before_native_allocation() {
        let limits = Limits::default();
        assert_eq!(
            limits.validate_value(Format::TextUtf8, "Привет\n✓".as_bytes()),
            Ok(())
        );
        assert_eq!(
            limits.validate_value(Format::TextUtf8, &[0xff]),
            Err(Error::Text)
        );
        assert_eq!(limits.validate_value(Format::TextUtf8, b""), Ok(()));
        let mut header = vec![0u8; 33];
        header[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        header[8..12].copy_from_slice(&13u32.to_be_bytes());
        header[12..16].copy_from_slice(b"IHDR");
        header[16..20].copy_from_slice(&1u32.to_be_bytes());
        header[20..24].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(limits.validate_value(Format::ImagePng, &header), Ok(()));
        header[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            limits.validate_value(Format::ImagePng, &header),
            Err(Error::TooLarge)
        );
        assert_eq!(
            limits.validate_value(Format::ImagePng, b"arbitrary bytes"),
            Err(Error::Image)
        );
    }

    proptest::proptest! {
        #[test]
        fn negotiated_limits_never_increase_either_peers_budget(text in 1..=TEXT_BYTES, image in 1..=IMAGE_BYTES, chunk in 1..=CHUNK_BYTES) {
            let peer=Limits{text_bytes:text,image_bytes:image,chunk_bytes:chunk,..Limits::default()};
            let local=Limits::default();
            let negotiated=local.intersect(&peer).unwrap();
            proptest::prop_assert!(negotiated.text_bytes<=peer.text_bytes && negotiated.text_bytes<=local.text_bytes);
            proptest::prop_assert!(negotiated.image_bytes<=peer.image_bytes && negotiated.image_bytes<=local.image_bytes);
            proptest::prop_assert!(negotiated.chunk_bytes<=peer.chunk_bytes && negotiated.chunk_bytes<=local.chunk_bytes);
        }
    }
}
