//! Explicit text paste, bounded reassembly and session-owned OS clipboard.
use crate::DesktopError;
use std::{future::Future, pin::Pin};
pub mod image;
#[cfg(all(target_os = "macos", any(feature = "viewer", feature = "host")))]
pub(crate) mod macos;
pub mod native;
pub(crate) mod ownership;
pub mod stream;
pub const MAX_TEXT_BYTES: usize = rds_core::clipboard::TEXT_BYTES as usize;
pub const CHUNK_BYTES: usize = 32 * 1024;
pub const SEND_CHUNK_BYTES: usize = 16 * 1024;

/// Async clipboard lease belonging to an already consented serving backend.
/// Native objects stay behind this interface; text is never logged.
pub type ClipboardFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// The caller's original scope/key/budget owner. Native jobs retain it through
/// actual I/O drain, including when their waiting future is canceled.
pub type NativeLifetime = std::sync::Arc<dyn Send + Sync>;
pub trait ClipboardBackend: Send {
    fn retain(&mut self, context: NativeLifetime) -> bool;
    fn close(&mut self) -> ClipboardFuture<'_, ()>;
    fn publish(&mut self, text: String) -> ClipboardFuture<'_, Result<(), DesktopError>>;
    fn next_change(&mut self) -> ClipboardFuture<'_, Option<String>>;
}
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A native clipboard value observed after the remote owner changed.  The
/// value is delivered to the session loop only after the same 1 MiB bound
/// used for incoming paste data has been enforced by the backend.
pub(crate) struct ClipboardChange {
    pub(crate) text: String,
}

struct Partial {
    id: u64,
    total: usize,
    bytes: Vec<u8>,
    started: std::time::Instant,
    progressed: std::time::Instant,
}

#[derive(Default)]
pub struct Assembly {
    partial: Option<Partial>,
}
impl Assembly {
    pub fn push(
        &mut self,
        id: u64,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    ) -> Result<Option<String>, DesktopError> {
        let bad =
            |reason: &str| DesktopError::Input(format!("clipboard transfer refused: {reason}"));
        let total = total as usize;
        if total > MAX_TEXT_BYTES || data.len() > CHUNK_BYTES {
            self.partial = None;
            return Err(bad("size bound exceeded"));
        }
        if offset == 0 {
            if self.partial.is_some() {
                self.partial = None;
                return Err(bad("overlapping transfer"));
            }
            let now = std::time::Instant::now();
            self.partial = Some(Partial {
                id,
                total,
                bytes: Vec::with_capacity(total),
                started: now,
                progressed: now,
            });
        }
        let Some(partial) = &mut self.partial else {
            return Err(bad("missing initial chunk"));
        };
        let reason = if partial.id != id || partial.total != total {
            Some("identity or length changed")
        } else if partial.bytes.len() != offset as usize || partial.bytes.len() + data.len() > total
        {
            Some("invalid chunk offset")
        } else if partial.started.elapsed() > TRANSFER_TIMEOUT {
            Some("total deadline exceeded")
        } else if partial.progressed.elapsed() > IDLE_TIMEOUT {
            Some("idle deadline exceeded")
        } else if data.is_empty() && total != 0 {
            Some("empty progress chunk")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.partial = None;
            return Err(bad(reason));
        }
        partial.bytes.extend_from_slice(&data);
        partial.progressed = std::time::Instant::now();
        if partial.bytes.len() != total {
            return Ok(None);
        }
        let partial = self.partial.take().expect("complete transfer exists");
        String::from_utf8(partial.bytes)
            .map(Some)
            .map_err(|_| bad("invalid UTF-8"))
    }
}

/// Session-scoped outbound clipboard state. Owns at most one offered value
/// and one requested transfer; a stale request never consumes the fresh offer.
#[derive(Default)]
pub(crate) struct ReverseSender {
    offer: Option<(u64, String, std::time::Instant)>,
    outgoing: Option<(u64, Vec<u8>, usize)>,
}
impl ReverseSender {
    pub(crate) fn offer(&mut self, text: String) -> Option<rds_core::DesktopEvent> {
        if text.len() > MAX_TEXT_BYTES {
            return None;
        }
        let id = rand::random();
        let bytes = text.len() as u32;
        self.offer = Some((id, text, std::time::Instant::now()));
        Some(rds_core::DesktopEvent::ClipboardOffer {
            id,
            format: rds_core::ClipboardFormat::TextUtf8,
            bytes,
        })
    }
    pub(crate) fn request(&mut self, id: u64) -> Result<(), rds_core::ClipboardErrorCode> {
        let Some((offered_id, _, at)) = &self.offer else {
            return Err(rds_core::ClipboardErrorCode::Expired);
        };
        if *offered_id != id {
            return Err(rds_core::ClipboardErrorCode::InvalidRequest);
        }
        if at.elapsed() >= TRANSFER_TIMEOUT {
            self.offer = None;
            return Err(rds_core::ClipboardErrorCode::Expired);
        }
        if self.outgoing.is_some() {
            return Err(rds_core::ClipboardErrorCode::Unavailable);
        }
        if let Some((id, text, _)) = self.offer.take() {
            self.outgoing = Some((id, text.into_bytes(), 0));
        }
        Ok(())
    }
    pub(crate) fn sending(&self) -> bool {
        self.outgoing.is_some()
    }
    pub(crate) fn invalidate_offer(&mut self) {
        self.offer = None;
    }
    pub(crate) fn next_chunk(&mut self) -> Option<rds_core::DesktopEvent> {
        let (id, bytes, offset) = self.outgoing.as_mut()?;
        let end = (*offset + SEND_CHUNK_BYTES).min(bytes.len());
        let chunk = rds_core::DesktopEvent::ClipboardChunk {
            id: *id,
            offset: *offset as u32,
            total: bytes.len() as u32,
            data: bytes[*offset..end].to_vec(),
        };
        *offset = end;
        if end == bytes.len() {
            self.outgoing = None;
        }
        Some(chunk)
    }
}

#[cfg(all(target_os = "linux", feature = "x11"))]
mod x11;
enum Backend {
    #[cfg(all(target_os = "linux", feature = "x11"))]
    X11(x11::Worker),
    Prepared(Box<dyn ClipboardBackend>),
    Unavailable,
}
pub(crate) struct Worker(Backend);
impl Worker {
    pub(crate) fn for_source(
        source: Option<&dyn crate::DesktopSource>,
        display: u32,
        watching: bool,
        lifetime: Option<NativeLifetime>,
    ) -> Self {
        let mut worker = Self::create(source, display, watching);
        if let Some(context) = lifetime {
            let retained = match &mut worker.0 {
                #[cfg(all(target_os = "linux", feature = "x11"))]
                Backend::X11(worker) => worker.retain(context),
                Backend::Prepared(worker) => worker.retain(context),
                Backend::Unavailable => false,
            };
            if !retained {
                return Self(Backend::Unavailable);
            }
        }
        worker
    }
    fn create(source: Option<&dyn crate::DesktopSource>, display: u32, watching: bool) -> Self {
        if let Some(source) = source {
            return Self(match source.clipboard(display, watching) {
                Ok(backend) => Backend::Prepared(backend),
                Err(_) => Backend::Unavailable,
            });
        }
        #[cfg(all(target_os = "linux", feature = "x11"))]
        {
            Self(Backend::X11(if watching {
                x11::Worker::watch(display)
            } else {
                x11::Worker::new(display)
            }))
        }
        #[cfg(not(all(target_os = "linux", feature = "x11")))]
        {
            let _ = (display, watching);
            Self(Backend::Unavailable)
        }
    }
    pub(crate) async fn publish(&mut self, text: String) -> Result<(), DesktopError> {
        match &mut self.0 {
            #[cfg(all(target_os = "linux", feature = "x11"))]
            Backend::X11(worker) => worker.publish(text).await,
            Backend::Prepared(worker) => worker.publish(text).await,
            Backend::Unavailable => {
                Err(DesktopError::Input("clipboard backend unavailable".into()))
            }
        }
    }
    pub(crate) async fn next_change(&mut self) -> Option<ClipboardChange> {
        match &mut self.0 {
            #[cfg(all(target_os = "linux", feature = "x11"))]
            Backend::X11(worker) => worker.next_change().await,
            Backend::Prepared(worker) => worker
                .next_change()
                .await
                .map(|text| ClipboardChange { text }),
            Backend::Unavailable => None,
        }
    }
    pub(crate) async fn close(&mut self) {
        match &mut self.0 {
            #[cfg(all(target_os = "linux", feature = "x11"))]
            Backend::X11(worker) => worker.close().await,
            Backend::Prepared(worker) => worker.close().await,
            Backend::Unavailable => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reverse_requests_are_consumed_once_and_stale_ids_preserve_the_latest_offer() {
        use rds_core::{ClipboardErrorCode, DesktopEvent};
        let mut sender = ReverseSender::default();
        let DesktopEvent::ClipboardOffer { id, .. } =
            sender.offer("a".repeat(SEND_CHUNK_BYTES + 3)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            sender.request(id.wrapping_add(1)),
            Err(ClipboardErrorCode::InvalidRequest)
        );
        sender.request(id).unwrap();
        assert_eq!(sender.request(id), Err(ClipboardErrorCode::Expired));
        let DesktopEvent::ClipboardChunk {
            offset,
            total,
            data,
            ..
        } = sender.next_chunk().unwrap()
        else {
            panic!()
        };
        assert_eq!(offset, 0);
        assert_eq!(total as usize, SEND_CHUNK_BYTES + 3);
        assert_eq!(data.len(), SEND_CHUNK_BYTES);
        let DesktopEvent::ClipboardChunk { offset, data, .. } = sender.next_chunk().unwrap() else {
            panic!()
        };
        assert_eq!(offset as usize, SEND_CHUNK_BYTES);
        assert_eq!(data, b"aaa");
        assert!(!sender.sending());
        let DesktopEvent::ClipboardOffer { id, .. } = sender.offer(String::new()).unwrap() else {
            panic!()
        };
        sender.request(id).unwrap();
        assert!(
            matches!(sender.next_chunk(), Some(DesktopEvent::ClipboardChunk { total: 0, offset: 0, data, .. }) if data.is_empty())
        );
        assert!(sender.offer("a".repeat(MAX_TEXT_BYTES + 1)).is_none());
        let DesktopEvent::ClipboardOffer { id, .. } = sender.offer("new".into()).unwrap() else {
            panic!()
        };
        sender.offer.as_mut().unwrap().2 -= std::time::Duration::from_secs(31);
        assert_eq!(sender.request(id), Err(ClipboardErrorCode::Expired));
    }
    #[test]
    fn progressing_transfer_survives_original_deadline_but_idle_and_total_are_bounded() {
        let mut assembly = Assembly::default();
        assembly.push(1, 0, 3, vec![b'a']).unwrap();
        assembly.partial.as_mut().unwrap().started -= std::time::Duration::from_secs(6);
        assert!(assembly.push(1, 1, 3, vec![b'b']).unwrap().is_none());
        assert_eq!(
            assembly.push(1, 2, 3, vec![b'c']).unwrap(),
            Some("abc".into())
        );
        assembly.push(2, 0, 2, vec![b'a']).unwrap();
        assembly.partial.as_mut().unwrap().progressed -= std::time::Duration::from_secs(6);
        assert!(
            assembly
                .push(2, 1, 2, vec![b'b'])
                .unwrap_err()
                .to_string()
                .contains("idle deadline")
        );
        assert!(assembly.partial.is_none());
        assembly.push(3, 0, 2, vec![b'a']).unwrap();
        assembly.partial.as_mut().unwrap().started -= std::time::Duration::from_secs(31);
        assert!(
            assembly
                .push(3, 1, 2, vec![b'b'])
                .unwrap_err()
                .to_string()
                .contains("total deadline")
        );
        assert!(assembly.partial.is_none());
        assert!(assembly.push(4, 0, 1, vec![]).is_err());
        assert_eq!(assembly.push(5, 0, 0, vec![]).unwrap(), Some(String::new()));
    }
    #[test]
    fn split_utf8_is_preserved_and_bad_offsets_or_size_fail_closed() {
        let text = "Привет 🖥️\nSecond line".as_bytes();
        let mut a = Assembly::default();
        assert!(
            a.push(1, 0, text.len() as u32, text[..3].to_vec())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            a.push(1, 3, text.len() as u32, text[3..].to_vec()).unwrap(),
            Some(String::from_utf8(text.to_vec()).unwrap())
        );
        assert!(a.push(2, 0, MAX_TEXT_BYTES as u32 + 1, vec![]).is_err());
        assert!(a.push(2, 0, 8, vec![1, 2]).unwrap().is_none());
        assert!(a.push(2, 3, 8, vec![3]).is_err());
        assert!(a.push(3, 0, 1, vec![0xff]).is_err());
    }
}
