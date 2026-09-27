//! Protocol types shared by every rds peer.
//!
//! Layout: one QUIC connection per peer pair (`ALPN = rds/0`), every
//! bi-directional stream opens with a length-prefixed postcard
//! [`StreamHello`], every uni-directional stream that carries a video frame
//! opens with a [`FrameHeader`]. Control messages on an established desktop
//! control stream are [`DesktopControl`] frames.

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Display index to capture.
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
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Server→client messages on the desktop control stream (v2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DesktopEvent {
    /// One input event was injected (sent when `DesktopHello::input_acks`).
    InputAck { seq: u64, handled_ts_ms: u64 },
    /// Echo of [`DesktopControl::Heartbeat`].
    Heartbeat { seq: u64, ts_ms: u64 },
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
    }
}
