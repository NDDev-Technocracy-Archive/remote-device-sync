//! Versioned local control protocol. This is not a remote service ALPN.
use serde::{Deserialize, Serialize};

/// Wire version 5 adds the managed desktop channel (`Desktop`,
/// `DesktopDown`/`DesktopUp`).
pub const VERSION: u16 = 5;
/// Explicit read-only capability profile. Its request uses the frozen List
/// command so a v5 manager can return its typed version refusal before work.
pub const CAPABILITIES_VERSION: u16 = 6;
pub const MAX_SESSIONS: usize = 32;
pub const TCP_CHUNK: usize = 16 * 1024;
/// Bound on one encoded desktop frame payload on the local channel — the
/// same 32 MiB the remote frame reader enforces.
pub const MAX_DESKTOP_PAYLOAD: usize = 32 * 1024 * 1024;

/// Local TCP bodies distinguish byte-direction FIN from caller departure.
/// Never Debug: application bytes can contain secrets.
#[derive(Serialize, Deserialize)]
pub enum TcpFrame {
    Data(Vec<u8>),
    Finish,
}

/// Manager→viewer messages on a [`Reply::DesktopOpened`] channel: encoded
/// frames exactly as the remote agent sent them (the viewer owns decoding),
/// control-plane events, and an explicit clean end marker.
///
/// A `Frame` postcard message is followed on the wire by a big-endian u32
/// length and that many encoded payload bytes (≤ `MAX_DESKTOP_PAYLOAD`) —
/// the same header-then-body shape the remote frame stream uses, since one
/// postcard message cannot hold a 32 MiB payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DesktopDown {
    /// One encoded frame past the remote sequence/stale-drop discipline.
    Frame {
        header: crate::FrameHeader,
    },
    Event(crate::DesktopEvent),
    /// The remote session ended; the socket closes after this frame.
    Finished,
}

/// Viewer→manager messages on a desktop channel. `Control` is forwarded
/// verbatim to the remote session's control stream; `Finished` ends the
/// relay without error.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DesktopUp {
    Control(crate::DesktopControl),
    Finished,
}

/// Random, process-lifetime handle. Never aliases a session after agent restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(pub [u8; 16]);

impl TryFrom<String> for SessionId {
    type Error = ErrorCode;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.to_string()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for SessionId {
    type Err = ErrorCode;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ErrorCode::InvalidRequest);
        }
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(s.as_bytes().as_chunks::<2>().0) {
            let digit = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b.to_ascii_lowercase() - b'a' + 10
                }
            };
            *byte = digit(pair[0]) * 16 + digit(pair[1]);
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub command: Command,
}

/// Deliberately not Debug: a connect command can carry a credential.
#[derive(Clone, Serialize, Deserialize)]
pub enum Command {
    List,
    Connect {
        target: String,
        grant: Option<Box<crate::grant::Grant>>,
    },
    Select {
        session: SessionId,
    },
    Disconnect {
        session: SessionId,
    },
    Ping {
        session: Option<SessionId>,
        nonce: u64,
    },
    Info {
        session: Option<SessionId>,
    },
    /// An Opened reply is followed by framed TCP data and explicit direction FIN.
    OpenTcp {
        session: Option<SessionId>,
        target: crate::TcpTarget,
    },
    /// Current addresses of the already-bound agent endpoint; no relay wait.
    Ticket,
    /// Extend one explicitly pinned grant-backed session without reconnecting.
    Renew {
        session: SessionId,
        grant: Box<crate::grant::Grant>,
    },
    /// Agent-owned single-file transfer. Local paths are absolute UTF-8 paths;
    /// the authenticated same-UID caller explicitly delegates filesystem I/O.
    Sync {
        session: SessionId,
        operation: SyncOperation,
    },
    /// Agent-mediated desktop viewer session. A `DesktopOpened` reply is
    /// followed by a bidirectional [`DesktopDown`]/[`DesktopUp`] channel on
    /// the same socket until either side ends it.
    Desktop {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
    },
    /// Explicit profile; old managers reject this additive command. The
    /// original desktop command/wire shape remains unchanged.
    DesktopProfile {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
        output_height: u32,
    },
    /// Additive v5 extension: video/control on this socket, events on a
    /// separately attached same-UID socket. Older managers refuse it.
    DesktopSeparated {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
        output_height: u32,
    },
    /// Claim one process-lifetime event route exactly once.
    DesktopEvents {
        route: SessionId,
    },
    /// Explicit validated-payload receipts plus the existing separated event
    /// socket. Both the manager and remote agent must support this extension.
    DesktopSeparatedReceipts {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
        output_height: u32,
    },
    /// Validated-payload receipts with the original combined desktop socket.
    DesktopProfileReceipts {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
        output_height: u32,
    },
    /// Explicit V5 clipboard session with separated media/control events.
    /// Older managers refuse this additive command. Legacy APIs stay V2–V4.
    DesktopFeatures {
        session: Option<SessionId>,
        hello: Box<crate::DesktopHello>,
        output_height: u32,
        payload_receipts: bool,
        clipboard: bool,
    },
    /// Bounded direct-address preference for an explicitly pinned peer.
    /// Authenticate its key before service admission; failed dialing falls
    /// back to ordinary resolution. Older managers refuse this extension.
    ConnectPreferred {
        target: String,
        grant: Option<Box<crate::grant::Grant>>,
        local_addresses: Vec<std::net::SocketAddr>,
    },
}

/// Address hints are reachability candidates, never an identity authority.
pub fn validate_local_addresses(addresses: &[std::net::SocketAddr]) -> Result<(), ErrorCode> {
    if addresses.len() > 4 {
        return Err(ErrorCode::InvalidRequest);
    }
    let mut unique = std::collections::BTreeSet::new();
    for address in addresses {
        let ip = address.ip();
        if address.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast())
            || !unique.insert(*address)
        {
            return Err(ErrorCode::InvalidRequest);
        }
    }
    Ok(())
}

/// Never Debug: paths may contain private information.
#[derive(Clone, Serialize, Deserialize)]
pub enum SyncOperation {
    Send { path: String },
    Recv { rel_path: String, directory: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncStats {
    pub fetched: u64,
    pub total: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Connecting,
    Connected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub peer: String,
    pub status: Status,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub instance: SessionId,
    pub generation: u64,
    pub endpoint: String,
    pub selected: Option<SessionId>,
    pub sessions: Vec<Session>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub result: Result<Reply, ErrorCode>,
}

/// Manager code support, not remote service authority or native OS consent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerCapabilities {
    pub instance: SessionId,
    /// True only once the native provider proxy/lease RPC is implemented.
    pub native_clipboard: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Reply {
    Snapshot(Snapshot),
    Connected(SessionId),
    Done,
    Pong {
        session: SessionId,
        micros: u64,
    },
    Info {
        session: SessionId,
        info: crate::AgentInfo,
    },
    Opened(SessionId),
    /// Desktop relay established: negotiated caps, then the socket becomes a
    /// [`DesktopDown`]/[`DesktopUp`] channel for the returned session.
    DesktopOpened {
        session: SessionId,
        caps: crate::DesktopCaps,
    },
    Ticket(String),
    Synced {
        session: SessionId,
        stats: SyncStats,
    },
    DesktopSeparatedOpened {
        session: SessionId,
        caps: crate::DesktopCaps,
        route: SessionId,
    },
    DesktopEventsOpened {
        route: SessionId,
    },
    /// Emitted only in the explicit capability profile, never to v5 callers.
    Capabilities(ManagerCapabilities),
}

/// Stable reasons, with no upstream error, address, path or credential payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ErrorCode {
    #[error("unsupported local protocol version")]
    Version,
    #[error("invalid local request")]
    InvalidRequest,
    #[error("session or worker capacity reached")]
    Capacity,
    #[error("connection to this peer is already pending")]
    Busy,
    #[error("peer already connected with different credentials; disconnect it first")]
    CredentialConflict,
    #[error("session is no longer available")]
    NotFound,
    #[error("no device selected; connect or select a session first")]
    NoSelection,
    #[error("target resolution failed; use a pinned ticket or key, or configure directory trust")]
    Resolve,
    #[error("peer connection or authorization failed")]
    Connect,
    #[error("remote operation failed or was denied")]
    Remote,
    #[error("local request deadline exceeded")]
    Timeout,
    #[error("manager is stopping")]
    Stopped,
    #[error("manager state unavailable")]
    Internal,
    #[error("another file transfer is active on this session")]
    TransferBusy,
    #[error("file transfer failed; a started commit may still complete; reconcile before retrying")]
    Transfer,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_address_hints_are_bounded_and_unicast() {
        for invalid in [
            "0.0.0.0:1",
            "127.0.0.1:0",
            "255.255.255.255:1",
            "224.0.0.1:1",
            "[::]:1",
            "[ff02::1]:1",
        ] {
            assert!(validate_local_addresses(&[invalid.parse().unwrap()]).is_err());
        }
        let valid = "127.0.0.1:1234".parse().unwrap();
        assert!(validate_local_addresses(&[valid]).is_ok());
        assert!(validate_local_addresses(&[valid, valid]).is_err());
        assert!(
            validate_local_addresses(
                &(1..=5)
                    .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    #[test]
    fn handles_and_validated_tcp_targets_round_trip() {
        let id = SessionId([0xa5; 16]);
        assert_eq!(id.to_string().parse::<SessionId>().unwrap(), id);
        assert_eq!(
            id.to_string().to_uppercase().parse::<SessionId>().unwrap(),
            id
        );
        for invalid in [
            "",
            "a",
            "gggggggggggggggggggggggggggggggg",
            "éééééééééééééééé",
        ] {
            assert!(invalid.parse::<SessionId>().is_err());
        }
        let request = Request {
            version: VERSION,
            command: Command::OpenTcp {
                session: Some(id),
                target: "[::ffff:127.0.0.1]:22".parse().unwrap(),
            },
        };
        let bytes = postcard::to_stdvec(&request).unwrap();
        let decoded: Request = postcard::from_bytes(&bytes).unwrap();
        match decoded.command {
            Command::OpenTcp { session, target } => {
                assert_eq!(session, Some(id));
                assert_eq!(target.to_string(), "127.0.0.1:22");
            }
            _ => panic!("wrong variant"),
        }
        for invalid in ["host:0", ":22", "[::]:22", "host:nope"] {
            let encoded = postcard::to_stdvec(invalid).unwrap();
            assert!(postcard::from_bytes::<crate::TcpTarget>(&encoded).is_err());
        }
    }

    #[test]
    fn desktop_channel_round_trips() {
        // Wire v5: managed desktop command, reply and body enums.
        assert_eq!(VERSION, 5);
        let id = SessionId([0x5a; 16]);
        let request = Request {
            version: VERSION,
            command: Command::Desktop {
                session: Some(id),
                hello: Box::new(crate::DesktopHello {
                    display: 1,
                    max_fps: 30,
                    codec: crate::Codec::H264,
                    input_acks: true,
                }),
            },
        };
        let bytes = postcard::to_stdvec(&request).unwrap();
        match postcard::from_bytes::<Request>(&bytes).unwrap().command {
            Command::Desktop { session, hello } => {
                assert_eq!(session, Some(id));
                assert_eq!(hello.display, 1);
                assert!(hello.input_acks);
            }
            _ => panic!("wrong variant"),
        }
        let response = Response {
            version: VERSION,
            result: Ok(Reply::DesktopOpened {
                session: id,
                caps: crate::DesktopCaps {
                    displays: vec![crate::DisplayInfo {
                        index: 0,
                        width: 1920,
                        height: 1080,
                        primary: true,
                    }],
                    codecs: vec![crate::Codec::H264],
                },
            }),
        };
        let bytes = postcard::to_stdvec(&response).unwrap();
        match postcard::from_bytes::<Response>(&bytes)
            .unwrap()
            .result
            .unwrap()
        {
            Reply::DesktopOpened { session, caps } => {
                assert_eq!(session, id);
                assert_eq!(caps.displays.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
        let header = crate::FrameHeader {
            seq: 7,
            capture_ts_ms: 1,
            encode_done_ts_ms: 2,
            send_ts_ms: 3,
            keyframe: true,
            codec: crate::Codec::H264,
            width: 640,
            height: 480,
        };
        for down in [
            DesktopDown::Frame { header },
            DesktopDown::Event(crate::DesktopEvent::Heartbeat { seq: 9, ts_ms: 4 }),
            DesktopDown::Finished,
        ] {
            let bytes = postcard::to_stdvec(&down).unwrap();
            assert!(bytes.len() < crate::MAX_MESSAGE_LEN as usize);
            postcard::from_bytes::<DesktopDown>(&bytes).unwrap();
        }
        for up in [
            DesktopUp::Control(crate::DesktopControl::RequestIdr),
            DesktopUp::Control(crate::DesktopControl::Heartbeat { seq: 3, ts_ms: 11 }),
            DesktopUp::Finished,
        ] {
            let bytes = postcard::to_stdvec(&up).unwrap();
            postcard::from_bytes::<DesktopUp>(&bytes).unwrap();
        }
    }

    #[test]
    fn separated_desktop_extension_preserves_existing_discriminants() {
        let id = SessionId([3; 16]);
        let hello = || {
            Box::new(crate::DesktopHello {
                display: 0,
                max_fps: 60,
                codec: crate::Codec::H264,
                input_acks: true,
            })
        };
        for (command, tag) in [
            (
                Command::ConnectPreferred {
                    target: "fixture-key".into(),
                    grant: None,
                    local_addresses: vec!["127.0.0.1:1234".parse().unwrap()],
                },
                17,
            ),
            (Command::Ticket, 7),
            (
                Command::Desktop {
                    session: Some(id),
                    hello: hello(),
                },
                10,
            ),
            (
                Command::DesktopProfile {
                    session: Some(id),
                    hello: hello(),
                    output_height: 1080,
                },
                11,
            ),
            (
                Command::DesktopSeparated {
                    session: Some(id),
                    hello: hello(),
                    output_height: 1080,
                },
                12,
            ),
            (Command::DesktopEvents { route: id }, 13),
            (
                Command::DesktopFeatures {
                    session: Some(id),
                    hello: hello(),
                    output_height: 1080,
                    payload_receipts: true,
                    clipboard: true,
                },
                16,
            ),
            (
                Command::DesktopSeparatedReceipts {
                    session: Some(id),
                    hello: hello(),
                    output_height: 1080,
                },
                14,
            ),
            (
                Command::DesktopProfileReceipts {
                    session: Some(id),
                    hello: hello(),
                    output_height: 1080,
                },
                15,
            ),
        ] {
            let bytes = postcard::to_stdvec(&command).unwrap();
            assert_eq!(bytes[0], tag);
            postcard::from_bytes::<Command>(&bytes).unwrap();
        }
        let caps = || crate::DesktopCaps {
            displays: vec![],
            codecs: vec![crate::Codec::H264],
        };
        for (reply, tag) in [
            (
                Reply::DesktopOpened {
                    session: id,
                    caps: caps(),
                },
                6,
            ),
            (
                Reply::DesktopSeparatedOpened {
                    session: id,
                    caps: caps(),
                    route: id,
                },
                9,
            ),
            (Reply::DesktopEventsOpened { route: id }, 10),
            (
                Reply::Capabilities(ManagerCapabilities {
                    instance: id,
                    native_clipboard: false,
                }),
                11,
            ),
        ] {
            let bytes = postcard::to_stdvec(&reply).unwrap();
            assert_eq!(bytes[0], tag);
            postcard::from_bytes::<Reply>(&bytes).unwrap();
        }
    }

    proptest::proptest! {
        #[test]
        fn local_decoders_never_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..1024), text in ".*") {
            let _ = postcard::from_bytes::<Request>(&bytes);
            let _ = postcard::from_bytes::<TcpFrame>(&bytes);
            let _ = postcard::from_bytes::<Response>(&bytes);
            let _ = postcard::from_bytes::<DesktopDown>(&bytes);
            let _ = postcard::from_bytes::<DesktopUp>(&bytes);
            let _ = text.parse::<SessionId>();
        }
    }
}
