//! Protocol types shared by every rds peer.
//!
//! Layout: one QUIC connection per peer pair (`ALPN = rds/0`), every
//! bi-directional stream opens with a length-prefixed postcard
//! [`StreamHello`], every uni-directional stream that carries a video frame
//! opens with a [`FrameHeader`]. Control messages on an established desktop
//! control stream are [`DesktopControl`] frames.

use serde::{Deserialize, Serialize};

pub mod clipboard;
/// Owned endpoint identity and addressing types (ed25519 keys,
/// relay/direct transport addresses) shared by both backends.
pub mod endpoint;
/// Capability grants (WS4): signed service-scope tokens presented on
/// the connection's first stream.
pub mod grant;
pub mod local;
/// Owned relay protocol wire types (ALPN `rds-relay/0`).
pub mod relay;
mod tcp_target;
pub use endpoint::{
    EndpointAddr, EndpointId, KeyParseError, PUBLIC_KEY_LENGTH, RelayUrl, SecretKey, TransportAddr,
};
pub use tcp_target::{TcpTarget, TcpTargetError};

/// ALPN negotiated for all rds traffic.
pub const ALPN: &[u8] = b"rds/0";

/// Wire protocol version. Peers refuse mismatched majors.
/// v2: FrameHeader carries capture/encode/send timestamps, InputEvent
/// carries metadata, control stream gains heartbeat + input acks.
/// v3: every uni-directional stream opens with a [`UniHello`] tag so a
/// single per-connection demux can route it — v2 consumers each called
/// `accept_uni` directly and could steal each other's streams.
pub const PROTOCOL_VERSION: u16 = 3;

/// Upper bound for a serialized greeting, guard against abusive peers.
pub const MAX_MESSAGE_LEN: u32 = 64 * 1024;

/// Desktop frame STOP_SENDING code: this predecessor is no longer needed
/// after a newer independent picture. It is neither fresh successful delivery
/// nor a broken current reference. Older senders may treat it as generic failure.
pub const DESKTOP_FRAME_OBSOLETE: u32 = 0x5244_5301;
/// Sender retires a DesktopV4 frame only after the peer proves complete payload receipt.
pub const DESKTOP_FRAME_RECEIVED: u32 = 0x5244_5302;

/// First frame on every uni-directional stream (v3): routes the stream
/// to the service that owns it. The accepting side runs one
/// `accept_uni` demux per connection and hands each stream to the
/// consumer registered for its tag — two services on one connection can
/// no longer consume each other's streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UniHello {
    /// Desktop video frame stream: a [`FrameHeader`] then the encoded
    /// payload follow.
    Desktop,
    /// Sync chunk stream: `SyncMsg` frames (`ChunkSet`, `ChunkHdr` +
    /// bytes, `SetDone`) follow.
    Sync,
    /// Audio packet stream: [`AudioFrame`] records follow (codec
    /// support lands in v0.3).
    Audio,
    /// Isolated file-transfer route. Never reuse an ID on a connection.
    SyncTransfer { id: [u8; 16] },
    /// Negotiated (v2) file-transfer route. Chunk streams and control
    /// frames both carry the transfer ID; older peers reject this tag
    /// at greeting decode, before any filesystem operation.
    SyncTransferV2 { id: [u8; 16] },
    /// Frame streams for one desktop session. The route ID is minted
    /// per session by the viewer and echoed in `StreamHello::DesktopV2`,
    /// so a delayed frame stream from an ended session can reach only
    /// the routing table — never a replacement session's inbox. Never
    /// reuse an ID on a connection.
    DesktopFrames { id: [u8; 16] },
    /// One explicitly requested native clipboard body. The owning desktop
    /// negotiates native clipboard support before claiming or sending it.
    ClipboardBody { id: clipboard::TransferId },
}

/// First frame on every bi-directional stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamHello {
    /// Round-trip probe; the peer echoes `nonce` back verbatim.
    Ping { nonce: u64 },
    /// Ask for peer metadata.
    Info,
    /// Splice this stream to a TCP socket on the serving side.
    TcpConnect { host: String, port: u16 },
    /// Open a desktop session control channel.
    Desktop(DesktopHello),
    /// Transfer a file or directory (WS6 sync protocol follows inside).
    Sync,
    /// Open an audio channel: one uni stream of [`AudioFrame`] records
    /// server→client on its own clock (codec support lands in v0.3).
    Audio(AudioHello),
    /// Present a capability [`grant::Grant`]. Must be the first stream
    /// on the connection when the agent runs in grant mode: every
    /// service received before authorization starts is refused. Services that
    /// overlap its reply/commit transaction wait for completion with a deadline.
    Authz(grant::Grant),
    /// Signed lease revision for this connection; same identity and exact scope.
    RenewAuthz(grant::Grant),
    /// File transfer with an isolated uni-stream route. Additive extension;
    /// older agents reject this greeting before filesystem operations.
    SyncTransfer { id: [u8; 16] },
    /// Negotiated file-transfer session: after [`HelloAck::Ok`] the
    /// control stream speaks the version-2 `SyncMsg::Session` envelope
    /// (transfer-ID bound frames, Hello/HelloAck limit negotiation,
    /// typed Cancel). Older agents reject the greeting before any
    /// filesystem operation; there is no silent version fallback.
    SyncTransferV2 { id: [u8; 16] },
    /// Desktop session with a per-session frame route: the server sends
    /// `UniHello::DesktopFrames { session }` frame streams. Older agents
    /// reject this greeting before any session work; there is no silent
    /// fallback to the shared `Desktop` route.
    DesktopV2 {
        session: [u8; 16],
        hello: DesktopHello,
    },
    /// Per-session video height. Zero preserves source resolution; positive
    /// heights are 16..=4320, aspect preserving and never upscale the source.
    /// Older agents reject this additive greeting without opening a session.
    DesktopV3 {
        session: [u8; 16],
        hello: DesktopHello,
        output_height: u32,
    },
    /// Explicit validated-payload receipts on the desktop control stream.
    /// Frame layout is unchanged. Older agents reject before session work;
    /// callers must not silently downgrade this requested mode.
    DesktopV4 {
        session: [u8; 16],
        hello: DesktopHello,
        output_height: u32,
    },
    /// Desktop session with explicit reverse text-clipboard negotiation.
    /// `payload_receipts` retains the DesktopV4 delivery proof in the same
    /// additive greeting; older agents refuse this mode before session work.
    DesktopV5 {
        session: [u8; 16],
        hello: DesktopHello,
        output_height: u32,
        payload_receipts: bool,
        clipboard: bool,
    },
}

/// Answer to a [`StreamHello`], sent before any service payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelloAck {
    /// Request accepted; the service payload follows on the same stream.
    Ok,
    /// Request rejected; the stream ends after this frame.
    Error { message: String },
    /// Answer to [`StreamHello::Info`].
    Info(AgentInfo),
    /// Answer to [`StreamHello::Desktop`].
    Desktop(DesktopCaps),
    /// DesktopV4 accepted with validated-payload receipts enabled.
    DesktopV4(DesktopCaps),
    /// DesktopV5 accepted with its explicitly negotiated extensions.
    DesktopV5(DesktopCaps),
}

/// What the serving side offers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub protocol: u16,
    pub version: String,
    pub hostname: Option<String>,
    pub services: Vec<ServiceKind>,
    pub desktop: Option<DesktopCaps>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ServiceKind {
    Ping,
    Info,
    Tcp,
    Desktop,
    /// Bulk file transfer (WS6).
    Sync,
    /// Audio forwarding (v0.3 codec; wire shape reserved in v2).
    Audio,
    /// Download files from the agent's configured sync root.
    SyncRead,
    /// Upload files to the agent's configured sync root.
    SyncWrite,
    /// View a desktop without injecting input.
    DesktopView,
    /// Input modifier: requires `DesktopView` to open a session.
    DesktopControl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopHello {
    /// Display ID from the serving peer's capability list. Legacy X-screen
    /// IDs retain their numbers; logical monitor IDs need not be contiguous.
    pub display: u32,
    /// Upper bound on produced frames per second.
    pub max_fps: u32,
    /// Requested codec.
    pub codec: Codec,
    /// Measurement mode: server acks each injected input event.
    pub input_acks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopCaps {
    pub displays: Vec<DisplayInfo>,
    pub codecs: Vec<Codec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Serving peer's display ID; the legacy field name/layout is preserved.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Clipboard representation negotiated on a desktop session.
///
/// The first wire extension intentionally carries text only.  Additional
/// formats must be added as new variants with their own bounds and native
/// conversion rules; treating arbitrary MIME bytes as text would make the
/// security and size contract ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardFormat {
    TextUtf8,
}

/// Typed failure for a reverse clipboard transfer.  The payload deliberately
/// contains no native error text or clipboard content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardErrorCode {
    Unavailable,
    UnsupportedFormat,
    TooLarge,
    Expired,
    InvalidRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// H.264 Annex-B, constrained baseline, no B-frames.
    H264,
}

/// Header at the start of every uni-directional video frame stream (v2).
///
/// All timestamps are milliseconds on the producer's session clock:
/// `capture_ts_ms` when the frame was captured, `encode_done_ts_ms` when
/// the encoder returned it, `send_ts_ms` when it was handed to the
/// transport — together they split pipeline delay from network delay,
/// which is what the latency budget is measured against.
///
/// The receiver resets a frame stream when a newer `seq` has already been
/// fully received: stale streams cost no further bandwidth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameHeader {
    pub seq: u64,
    pub capture_ts_ms: u64,
    pub encode_done_ts_ms: u64,
    pub send_ts_ms: u64,
    pub keyframe: bool,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

/// Client→server messages on the desktop control stream.
#[derive(Clone, Serialize, Deserialize)]
pub enum DesktopControl {
    /// Ask for a fresh IDR (after join or packet loss).
    RequestIdr,
    /// Ask the encoder to aim for this bitrate in bits per second.
    SetBitrate(u32),
    /// One input event to inject on the serving side.
    Input(InputEvent),
    /// Liveness probe; the server echoes it as [`DesktopEvent::Heartbeat`].
    /// Lets the viewer measure control-plane RTT under video backlog.
    Heartbeat { seq: u64, ts_ms: u64 },
    /// Explicit paste transfer. At most 1 MiB UTF-8 total and 32 KiB per
    /// chunk; ordered offsets, one active transfer per session. Contents are
    /// never diagnostics. The server publishes before later input is handled.
    ClipboardChunk {
        id: u64,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    },
    /// Complete, bounded encoded payload read through EOF on this session's
    /// frame route. BLAKE3 binds the receipt to the exact payload. This proves
    /// receipt, not decoding or presentation; enabled only by DesktopV4.
    FrameReceived {
        seq: u64,
        digest: [u8; 32],
        obsolete: bool,
    },
    /// Request the payload for a previously offered remote clipboard value.
    /// Requests are session-scoped and must match the latest offer ID.
    ClipboardRequest { id: u64, format: ClipboardFormat },
}

impl std::fmt::Debug for DesktopControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestIdr => f.write_str("RequestIdr"),
            Self::SetBitrate(rate) => f.debug_tuple("SetBitrate").field(rate).finish(),
            Self::Input(event) => f.debug_tuple("Input").field(event).finish(),
            Self::Heartbeat { seq, ts_ms } => f
                .debug_struct("Heartbeat")
                .field("seq", seq)
                .field("ts_ms", ts_ms)
                .finish(),
            Self::ClipboardChunk {
                id,
                offset,
                total,
                data,
            } => f
                .debug_struct("ClipboardChunk")
                .field("id", id)
                .field("offset", offset)
                .field("total", total)
                .field("bytes", &data.len())
                .finish(),
            Self::ClipboardRequest { id, format } => f
                .debug_struct("ClipboardRequest")
                .field("id", id)
                .field("format", format)
                .finish(),
            Self::FrameReceived { seq, .. } => f
                .debug_struct("FrameReceived")
                .field("seq", seq)
                .finish_non_exhaustive(),
        }
    }
}

/// Server→client messages on the desktop control stream (v2).
#[derive(Clone, Serialize, Deserialize)]
pub enum DesktopEvent {
    /// One input event was injected (sent when `DesktopHello::input_acks`).
    InputAck { seq: u64, handled_ts_ms: u64 },
    /// Echo of [`DesktopControl::Heartbeat`].
    Heartbeat { seq: u64, ts_ms: u64 },
    /// Clipboard is owned by the target selection service; no payload echoed.
    ClipboardReady { id: u64, bytes: u32 },
    /// A remote native clipboard changed.  The content is not included in
    /// the offer; the viewer explicitly requests it after applying policy.
    ClipboardOffer {
        id: u64,
        format: ClipboardFormat,
        bytes: u32,
    },
    /// One bounded chunk of a requested remote clipboard value.
    ClipboardChunk {
        id: u64,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    },
    /// Reverse clipboard transfer failed without exposing native details.
    ClipboardError { id: u64, code: ClipboardErrorCode },
}

impl std::fmt::Debug for DesktopEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputAck { seq, handled_ts_ms } => f
                .debug_struct("InputAck")
                .field("seq", seq)
                .field("handled_ts_ms", handled_ts_ms)
                .finish(),
            Self::Heartbeat { seq, ts_ms } => f
                .debug_struct("Heartbeat")
                .field("seq", seq)
                .field("ts_ms", ts_ms)
                .finish(),
            Self::ClipboardReady { id, bytes } => f
                .debug_struct("ClipboardReady")
                .field("id", id)
                .field("bytes", bytes)
                .finish(),
            Self::ClipboardOffer { id, format, bytes } => f
                .debug_struct("ClipboardOffer")
                .field("id", id)
                .field("format", format)
                .field("bytes", bytes)
                .finish(),
            Self::ClipboardChunk {
                id,
                offset,
                total,
                data,
            } => f
                .debug_struct("ClipboardChunk")
                .field("id", id)
                .field("offset", offset)
                .field("total", total)
                .field("bytes", &data.len())
                .finish(),
            Self::ClipboardError { id, code } => f
                .debug_struct("ClipboardError")
                .field("id", id)
                .field("code", code)
                .finish(),
        }
    }
}

/// One input event plus the metadata the serving side needs to route and
/// the viewer needs to correlate acks (v2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputEvent {
    /// Per-session sequence number, assigned by the viewer.
    pub seq: u64,
    /// When the viewer produced the event, ms on its own clock.
    pub event_ts_ms: u64,
    /// Display the event targets.
    pub display_id: u32,
    pub kind: InputKind,
}

/// The input action itself; carried inside [`InputEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputKind {
    /// Linux evdev key code, pressed.
    KeyDown { code: u32 },
    /// Linux evdev key code, released.
    KeyUp { code: u32 },
    /// Absolute pointer position in display coordinates.
    PointerMove { x: f64, y: f64 },
    /// Relative pointer motion.
    PointerMotion { dx: f64, dy: f64 },
    /// evdev button code.
    PointerButton { button: i32, pressed: bool },
    /// Scroll in fractional wheel steps: positive x is left, positive y is up.
    Scroll { dx: f64, dy: f64 },
}

/// Audio channel negotiation (`StreamHello::Audio`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioHello {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// Opus in Ogg-less framing; one [`AudioFrame`] record per packet.
    Opus,
}

/// One audio packet on the session's uni audio stream. `capture_ts_ms`
/// is on the audio device's own clock, independent of the video clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFrame {
    pub seq: u64,
    pub capture_ts_ms: u64,
    /// Samples per channel in this packet.
    pub samples: u32,
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn service_wire_tags_are_append_only_and_old_decoders_refuse_new_scopes() {
        use super::ServiceKind::*;
        #[derive(serde::Deserialize)]
        enum LegacyService {
            Ping,
            Info,
            Tcp,
            Desktop,
            Sync,
            Audio,
        }
        for (tag, kind) in [
            Ping,
            Info,
            Tcp,
            Desktop,
            Sync,
            Audio,
            SyncRead,
            SyncWrite,
            DesktopView,
            DesktopControl,
        ]
        .into_iter()
        .enumerate()
        {
            let bytes = postcard::to_stdvec(&kind).unwrap();
            assert_eq!(bytes, vec![tag as u8]);
            assert_eq!(postcard::from_bytes::<ServiceKind>(&bytes).unwrap(), kind);
            assert_eq!(
                postcard::from_bytes::<LegacyService>(&bytes).is_ok(),
                tag < 6
            );
        }
    }
    use super::*;

    #[test]
    fn clipboard_debug_contains_metadata_without_text_or_raw_bytes() {
        let text = "clipboard private payload";
        let chunk = DesktopControl::ClipboardChunk {
            id: 7,
            offset: 0,
            total: text.len() as u32,
            data: text.as_bytes().to_vec(),
        };
        let debug = format!("{chunk:?}");
        assert!(!debug.contains(text));
        assert!(!debug.contains(&format!("{:?}", text.as_bytes())));
        assert!(debug.contains("bytes"));
    }

    #[test]
    fn isolated_sync_appends_tags_and_legacy_decoders_refuse_the_extension() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        enum LegacyHello {
            Ping { nonce: u64 },
            Info,
            TcpConnect { host: String, port: u16 },
            Desktop(DesktopHello),
            Sync,
            Audio(AudioHello),
            Authz(grant::Grant),
            RenewAuthz(grant::Grant),
        }
        #[derive(serde::Deserialize)]
        enum LegacyUni {
            Desktop,
            Sync,
            Audio,
        }
        assert_eq!(postcard::to_stdvec(&StreamHello::Sync).unwrap(), [4]);
        assert_eq!(postcard::to_stdvec(&UniHello::Sync).unwrap(), [1]);
        let hello = postcard::to_stdvec(&StreamHello::SyncTransfer { id: [42; 16] }).unwrap();
        let uni = postcard::to_stdvec(&UniHello::SyncTransfer { id: [42; 16] }).unwrap();
        assert_eq!(hello, [vec![8], vec![42; 16]].concat());
        assert_eq!(uni, [vec![3], vec![42; 16]].concat());
        assert!(postcard::from_bytes::<LegacyHello>(&hello).is_err());
        assert!(postcard::from_bytes::<LegacyUni>(&uni).is_err());
        let hello2 = postcard::to_stdvec(&StreamHello::SyncTransferV2 { id: [43; 16] }).unwrap();
        let uni2 = postcard::to_stdvec(&UniHello::SyncTransferV2 { id: [43; 16] }).unwrap();
        assert_eq!(hello2, [vec![9], vec![43; 16]].concat());
        assert_eq!(uni2, [vec![4], vec![43; 16]].concat());
        assert!(postcard::from_bytes::<LegacyHello>(&hello2).is_err());
        assert!(postcard::from_bytes::<LegacyUni>(&uni2).is_err());
        let dv2 = postcard::to_stdvec(&StreamHello::DesktopV2 {
            session: [44; 16],
            hello: DesktopHello {
                display: 0,
                max_fps: 30,
                codec: Codec::H264,
                input_acks: false,
            },
        })
        .unwrap();
        let uf2 = postcard::to_stdvec(&UniHello::DesktopFrames { id: [44; 16] }).unwrap();
        let dv2_prefix = [&[10], [44; 16].as_slice()].concat();
        assert_eq!(&dv2[..17], dv2_prefix.as_slice());
        assert_eq!(uf2, [vec![5], vec![44; 16]].concat());
        assert!(postcard::from_bytes::<LegacyHello>(&dv2).is_err());
        assert!(postcard::from_bytes::<LegacyUni>(&uf2).is_err());
        let clipboard = UniHello::ClipboardBody {
            id: clipboard::TransferId {
                desktop: [45; 16],
                offer: 1,
                request: 2,
            },
        };
        let encoded = postcard::to_stdvec(&clipboard).unwrap();
        assert_eq!(encoded, [vec![6], vec![45; 16], vec![1, 2]].concat());
        assert_eq!(
            postcard::from_bytes::<UniHello>(&encoded).unwrap(),
            clipboard
        );
        assert!(postcard::from_bytes::<LegacyUni>(&encoded).is_err());
    }

    #[test]
    fn payload_receipts_append_wire_tags_and_redact_the_content_digest() {
        let hello = || DesktopHello {
            display: 0,
            max_fps: 60,
            codec: Codec::H264,
            input_acks: true,
        };
        let legacy = postcard::to_stdvec(&StreamHello::DesktopV3 {
            session: [1; 16],
            hello: hello(),
            output_height: 1080,
        })
        .unwrap();
        let new = postcard::to_stdvec(&StreamHello::DesktopV4 {
            session: [1; 16],
            hello: hello(),
            output_height: 1080,
        })
        .unwrap();
        assert_eq!(legacy[0], 11);
        assert_eq!(new[0], 12);
        assert_eq!(&legacy[1..], &new[1..]);
        let receipt = DesktopControl::FrameReceived {
            seq: 7,
            digest: [0xab; 32],
            obsolete: false,
        };
        let bytes = postcard::to_stdvec(&receipt).unwrap();
        assert_eq!(bytes[0], 5);
        assert!(
            matches!(postcard::from_bytes::<DesktopControl>(&bytes).unwrap(),DesktopControl::FrameReceived { seq:7,digest, obsolete:false } if digest==[0xab;32])
        );
        assert!(!format!("{receipt:?}").contains("digest"));
        let caps = || DesktopCaps {
            displays: vec![],
            codecs: vec![Codec::H264],
        };
        assert_eq!(
            postcard::to_stdvec(&HelloAck::Desktop(caps())).unwrap()[0],
            3
        );
        assert_eq!(
            postcard::to_stdvec(&HelloAck::DesktopV4(caps())).unwrap()[0],
            4
        );
        assert_eq!(
            postcard::to_stdvec(&StreamHello::DesktopV5 {
                session: [1; 16],
                hello: hello(),
                output_height: 1080,
                payload_receipts: true,
                clipboard: true,
            })
            .unwrap()[0],
            13
        );
        assert_eq!(
            postcard::to_stdvec(&HelloAck::DesktopV5(caps())).unwrap()[0],
            5
        );
    }

    #[test]
    fn reverse_clipboard_messages_are_append_only_and_redact_payloads() {
        let request = DesktopControl::ClipboardRequest {
            id: 91,
            format: ClipboardFormat::TextUtf8,
        };
        let request_bytes = postcard::to_stdvec(&request).unwrap();
        assert_eq!(request_bytes[0], 6);
        assert!(matches!(
            postcard::from_bytes::<DesktopControl>(&request_bytes).unwrap(),
            DesktopControl::ClipboardRequest {
                id: 91,
                format: ClipboardFormat::TextUtf8
            }
        ));

        let payload = b"private clipboard body".to_vec();
        let event = DesktopEvent::ClipboardChunk {
            id: 91,
            offset: 0,
            total: payload.len() as u32,
            data: payload.clone(),
        };
        let debug = format!("{event:?}");
        assert!(!debug.contains("private clipboard body"));
        assert!(!debug.contains(&format!("{payload:?}")));
        assert!(matches!(
            postcard::from_bytes::<DesktopEvent>(
                &postcard::to_stdvec(&DesktopEvent::ClipboardOffer {
                    id: 91,
                    format: ClipboardFormat::TextUtf8,
                    bytes: payload.len() as u32,
                })
                .unwrap()
            )
            .unwrap(),
            DesktopEvent::ClipboardOffer {
                id: 91,
                format: ClipboardFormat::TextUtf8,
                bytes: 22
            }
        ));
    }
}
