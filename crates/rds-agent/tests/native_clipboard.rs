//! Actual agent/desktop negotiation with an explicitly in-memory clipboard.
//! These tests do not establish native OS publication or application paste.
#![cfg(feature = "desktop")]

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rds_agent::{Agent, AgentPolicy};
use rds_core::{
    Codec, DesktopCaps, DesktopControl, DesktopEvent, DesktopHello, DisplayInfo, InputEvent,
    ServiceKind,
    clipboard::{Capabilities, Format, ShortcutModifier},
};
use rds_desktop::{
    DesktopError, DesktopSource, FrameProducer, InputSink, SyntheticProducer,
    client::{DesktopSession, NativeClipboard, SessionOpts},
    clipboard::{
        ClipboardFuture, NativeLifetime,
        native::{self, Provider},
    },
};
use rds_net::{Backend, Connection, Endpoint, EndpointConfig};
use tokio::sync::watch;

struct State {
    text: &'static str,
    reads: AtomicUsize,
    factories: AtomicUsize,
    legacy: AtomicUsize,
    inputs: AtomicUsize,
}
impl State {
    fn new(text: &'static str) -> Arc<Self> {
        Arc::new(Self {
            text,
            reads: 0.into(),
            factories: 0.into(),
            legacy: 0.into(),
            inputs: 0.into(),
        })
    }
}
struct Memory {
    state: Arc<State>,
    closed: watch::Sender<bool>,
    shortcut: ShortcutModifier,
    _original: NativeLifetime,
}
impl Memory {
    fn new(state: Arc<State>, shortcut: ShortcutModifier, original: NativeLifetime) -> Arc<Self> {
        let (closed, _) = watch::channel(false);
        Arc::new(Self {
            state,
            closed,
            shortcut,
            _original: original,
        })
    }
    fn snapshot(&self) -> native::Snapshot {
        native::Snapshot {
            generation: 1,
            owned: false,
            representations: vec![rds_core::clipboard::Representation {
                format: Format::TextUtf8,
                bytes: Some(self.state.text.len() as u64),
            }],
        }
    }
}
impl Provider for Memory {
    fn close(&self) -> ClipboardFuture<'_, ()> {
        self.closed.send_replace(true);
        Box::pin(async {})
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            receive: vec![Format::TextUtf8],
            offer: vec![Format::TextUtf8],
            shortcut: self.shortcut,
            limits: native::Policy::shipped().unwrap().limits,
        }
    }
    fn current(&self) -> native::Operation<'_, native::Snapshot> {
        Box::pin(async {
            if *self.closed.borrow() {
                Err(native::Error::Changed)
            } else {
                Ok(self.snapshot())
            }
        })
    }
    fn changed(&self, _: u64) -> native::Operation<'_, native::Snapshot> {
        Box::pin(async {
            let mut closed = self.closed.subscribe();
            let already_closed = *closed.borrow_and_update();
            if !already_closed {
                let _ = closed.changed().await;
            }
            Err(native::Error::Changed)
        })
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> native::Operation<'_, native::Value> {
        Box::pin(async move {
            let _original = original;
            if *self.closed.borrow() || generation != 1 {
                return Err(native::Error::Changed);
            }
            if format != Format::TextUtf8 {
                return Err(native::Error::Unsupported);
            }
            self.state.reads.fetch_add(1, Ordering::Relaxed);
            Ok(native::Value::Text(native::Text::new(
                self.state.text.as_bytes().to_vec().into(),
                &self.capabilities().limits,
            )?))
        })
    }
    fn publish(
        &self,
        _: u64,
        _: native::Publication,
        _: NativeLifetime,
    ) -> native::Operation<'_, native::Snapshot> {
        Box::pin(async { Err(native::Error::Unsupported) })
    }
}
struct Source {
    state: Arc<State>,
    native: bool,
}
struct Sink;
impl InputSink for Sink {
    fn inject(&mut self, _: &InputEvent) -> Result<(), DesktopError> {
        Ok(())
    }
}
impl DesktopSource for Source {
    fn capabilities(&self) -> Result<DesktopCaps, DesktopError> {
        Ok(DesktopCaps {
            codecs: vec![Codec::H264],
            displays: vec![DisplayInfo {
                index: 0,
                width: 32,
                height: 32,
                primary: true,
            }],
        })
    }
    fn producer(
        &self,
        _: u32,
        _: Duration,
        _: Option<u32>,
        _: Option<(u32, u32)>,
    ) -> Result<Box<dyn FrameProducer>, DesktopError> {
        Ok(Box::new(SyntheticProducer::new(5, 32, 32, 64)))
    }
    fn input(&self, _: u32, _: (u32, u32)) -> Result<Box<dyn InputSink>, DesktopError> {
        self.state.inputs.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Sink))
    }
    fn clipboard_supported(&self) -> bool {
        true
    }
    fn clipboard(
        &self,
        _: u32,
        _: bool,
    ) -> Result<Box<dyn rds_desktop::clipboard::ClipboardBackend>, DesktopError> {
        self.state.legacy.fetch_add(1, Ordering::Relaxed);
        Err(DesktopError::Input("legacy fixture must not open".into()))
    }
    fn native_clipboard(
        &self,
        _: u32,
        original: NativeLifetime,
    ) -> Result<Arc<dyn Provider>, DesktopError> {
        self.state.factories.fetch_add(1, Ordering::Relaxed);
        if !self.native {
            return Err(DesktopError::Input("native fixture unavailable".into()));
        }
        Ok(Memory::new(
            self.state.clone(),
            ShortcutModifier::Command,
            original,
        ))
    }
}
struct Runner(tokio::task::JoinHandle<()>);
impl Drop for Runner {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn endpoint(backend: Backend) -> Endpoint {
    rds_net::bind_endpoint(EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    })
    .await
    .unwrap()
}
fn backends() -> Vec<Backend> {
    #[cfg(feature = "transport-noq")]
    {
        vec![Backend::Iroh, Backend::Noq]
    }
    #[cfg(not(feature = "transport-noq"))]
    {
        vec![Backend::Iroh]
    }
}
fn options(provider: Arc<Memory>) -> SessionOpts {
    SessionOpts {
        session: Some(rand::random()),
        relay_encoded: true,
        payload_receipts: true,
        native_clipboard: Some(NativeClipboard {
            admitted: provider.capabilities(),
            provider,
            policy: native::Policy::shipped().unwrap().peer,
            original: Arc::new(()),
        }),
        ..Default::default()
    }
}
async fn desktop(conn: &Connection, provider: Arc<Memory>) -> DesktopSession {
    DesktopSession::connect_opts(
        conn,
        DesktopHello {
            display: 0,
            max_fps: 5,
            codec: Codec::H264,
            input_acks: false,
        },
        options(provider),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn native_desktop_negotiates_the_real_agent_without_opening_a_legacy_worker() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for backend in backends() {
            for (available, view_only) in [(true, false), (false, false), (true, true)] {
                let client = endpoint(backend).await;
                let server = endpoint(backend).await;
                let state = State::new("host 🖥️");
                let viewer_state = State::new("viewer ✓");
                let mut policy = AgentPolicy::ssh_only(("127.0.0.1".into(), 9));
                policy.allow.insert(client.id());
                policy.services = Some(BTreeSet::from([ServiceKind::Desktop]));
                let issuer = ed25519_dalek::SigningKey::from_bytes(&[29; 32]);
                if view_only {
                    policy.issuers.insert(issuer.verifying_key().to_bytes());
                    policy.use_local_revocations();
                }
                let agent = Arc::new(Agent::new(server.clone(), policy).with_desktop_source(
                    Arc::new(Source {
                        state: state.clone(),
                        native: available,
                    }),
                ));
                let running = agent.clone();
                let _runner = Runner(tokio::spawn(async move {
                    running.run().await.unwrap();
                }));
                let conn = if view_only {
                    let grant = rds_core::grant::Grant::issue(
                        &issuer,
                        *client.id().as_bytes(),
                        *server.id().as_bytes(),
                        [7; 16],
                        vec![ServiceKind::DesktopView, ServiceKind::Ping],
                        Duration::from_secs(120),
                        Default::default(),
                    );
                    rds_client::connect_authorized(&client, server.addr(), &grant)
                        .await
                        .unwrap()
                } else {
                    rds_client::connect(&client, server.addr()).await.unwrap()
                };
                let viewer = Memory::new(
                    viewer_state.clone(),
                    ShortcutModifier::Control,
                    Arc::new(()),
                );
                let mut session = desktop(&conn, viewer.clone()).await;
                assert_eq!(
                    state.factories.load(Ordering::Relaxed),
                    usize::from(!view_only)
                );
                assert_eq!(state.reads.load(Ordering::Relaxed), 0);
                assert_eq!(viewer_state.reads.load(Ordering::Relaxed), 0);
                if available && !view_only {
                    let handle = session.native_clipboard.as_ref().unwrap().clone();
                    assert_eq!(handle.peer_shortcut(), ShortcutModifier::Command);
                    let mut offers = handle.offers();
                    let offer = loop {
                        if let Some(value) = offers.borrow_and_update().clone() {
                            break value;
                        }
                        offers.changed().await.unwrap();
                    };
                    let incoming = handle
                        .request(offer.id, Format::TextUtf8, Default::default(), Arc::new(()))
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap();
                    assert_eq!(incoming.bytes().as_ref(), state.text.as_bytes());
                    assert_eq!(state.reads.load(Ordering::Relaxed), 1);
                    incoming.acknowledge(false).unwrap();
                    drop(incoming);
                    drop(session);
                    assert!(handle.closed());
                } else {
                    assert!(session.native_clipboard.is_none());
                    assert!(*viewer.closed.borrow());
                    if !view_only {
                        session
                            .send_control(DesktopControl::ClipboardChunk {
                                id: 9,
                                offset: 0,
                                total: 1,
                                data: vec![b'x'],
                            })
                            .await
                            .unwrap();
                        assert!(matches!(
                            session.events.recv().await,
                            Some(DesktopEvent::ClipboardError {
                                id: 9,
                                code: rds_core::ClipboardErrorCode::UnsupportedFormat
                            })
                        ));
                    }
                    session.heartbeat().await.unwrap();
                    assert!(matches!(
                        session.events.recv().await,
                        Some(DesktopEvent::Heartbeat { .. })
                    ));
                    if view_only {
                        assert_eq!(state.inputs.load(Ordering::Relaxed), 0);
                    }
                    drop(session);
                }
                assert_eq!(state.legacy.load(Ordering::Relaxed), 0);
                client.close().await;
                server.close().await;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_handshake_rejection_and_cancellation_release_only_their_own_routes_and_lease() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for backend in backends() {
            let client = endpoint(backend).await;
            let server = endpoint(backend).await;
            let (local, remote) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
                server.accept().await.unwrap().await
            });
            let local = local.unwrap();
            let remote = remote.unwrap();
            let hello = || DesktopHello { display:0, max_fps:5, codec:Codec::H264, input_acks:false };

            // A duplicate claim is refused before greeting I/O. The failed
            // caller may release only its new native inbox, not this owner.
            let id = rand::random();
            let original_route = local.uni_streams(rds_core::UniHello::DesktopFrames { id }).unwrap();
            let rejected = Memory::new(State::new("rejected"), ShortcutModifier::Control, Arc::new(()));
            let mut selected = options(rejected.clone());
            selected.session = Some(id);
            assert!(DesktopSession::connect_opts(&local, hello(), selected).await.is_err());
            assert!(*rejected.closed.borrow());
            assert_eq!(local.uni_routing_stats().routes, 1);
            drop(original_route);
            assert_eq!(local.uni_routing_stats().routes, 0);

            let canceled = Memory::new(State::new("canceled"), ShortcutModifier::Control, Arc::new(()));
            let mut opening = Box::pin(DesktopSession::connect_opts(&local, hello(), options(canceled.clone())));
            let control = tokio::select! {
                control = async {
                    let (send, mut recv) = remote.accept_bi().await.unwrap();
                    let greeting: rds_core::StreamHello = rds_net::read_frame(&mut recv).await.unwrap();
                    assert!(matches!(greeting, rds_core::StreamHello::DesktopV6 { .. }));
                    (send, recv)
                } => control,
                _ = &mut opening => panic!("native handshake completed without an acknowledgement"),
            };
            assert_eq!(local.uni_routing_stats().routes, 2);
            assert!(!*canceled.closed.borrow());
            assert_eq!(canceled.state.reads.load(Ordering::Relaxed), 0);
            drop(opening);
            assert!(*canceled.closed.borrow());
            assert_eq!(local.uni_routing_stats().routes, 0);
            drop(control);
            client.close().await;
            server.close().await;
        }
    }).await.unwrap();
}
