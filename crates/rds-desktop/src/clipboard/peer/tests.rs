//! Actual metadata/body transport; this provider is deliberately in-memory.
use super::*;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Clone)]
struct State {
    snapshot: Snapshot,
    format: Format,
    bytes: Bytes,
}
struct Memory {
    state: watch::Sender<State>,
    reads: AtomicUsize,
    closed: AtomicBool,
    paused: AtomicBool,
    metadata_paused: AtomicBool,
    entered: Notify,
    finished: Notify,
    release: Notify,
    shortcut: ShortcutModifier,
}
impl Memory {
    fn new(text: &'static str, shortcut: ShortcutModifier) -> Arc<Self> {
        let bytes = Bytes::from_static(text.as_bytes());
        let (state, _) = watch::channel(State {
            snapshot: Snapshot {
                generation: 1,
                owned: false,
                representations: vec![rds_core::clipboard::Representation {
                    format: Format::TextUtf8,
                    bytes: Some(bytes.len() as u64),
                }],
            },
            format: Format::TextUtf8,
            bytes,
        });
        Arc::new(Self {
            state,
            reads: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            metadata_paused: AtomicBool::new(false),
            entered: Notify::new(),
            finished: Notify::new(),
            release: Notify::new(),
            shortcut,
        })
    }
    fn change(&self, format: Format, bytes: Bytes) {
        let generation = self.state.borrow().snapshot.generation + 1;
        self.state.send_replace(State {
            snapshot: Snapshot {
                generation,
                owned: false,
                representations: vec![rds_core::clipboard::Representation {
                    format,
                    bytes: Some(bytes.len() as u64),
                }],
            },
            format,
            bytes,
        });
    }
}
impl Provider for Memory {
    fn close(&self) -> super::super::ClipboardFuture<'_, ()> {
        self.closed.store(true, Ordering::Release);
        self.release.notify_waiters();
        Box::pin(async {})
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            receive: vec![Format::TextUtf8, Format::ImagePng],
            offer: vec![Format::TextUtf8, Format::ImagePng],
            shortcut: self.shortcut,
            limits: native::Policy::shipped().unwrap().limits,
        }
    }
    fn current(&self) -> native::Operation<'_, Snapshot> {
        Box::pin(async {
            if self.metadata_paused.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            if self.closed.load(Ordering::Acquire) {
                Err(native::Error::Changed)
            } else {
                Ok(self.state.borrow().snapshot.clone())
            }
        })
    }
    fn changed(&self, after: u64) -> native::Operation<'_, Snapshot> {
        Box::pin(async move {
            let mut changes = self.state.subscribe();
            loop {
                let snapshot = changes.borrow_and_update().snapshot.clone();
                if snapshot.generation != after {
                    return Ok(snapshot);
                }
                changes
                    .changed()
                    .await
                    .map_err(|_| native::Error::Unavailable)?;
            }
        })
    }
    fn read(
        &self,
        generation: u64,
        format: Format,
        original: NativeLifetime,
    ) -> native::Operation<'_, Value> {
        Box::pin(async move {
            struct Finished<'a>(&'a Notify);
            impl Drop for Finished<'_> {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }
            let _original = original;
            let _finished = Finished(&self.finished);
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.entered.notify_one();
            if self.paused.load(Ordering::Acquire) {
                self.release.notified().await;
            }
            let state = self.state.borrow().clone();
            if self.closed.load(Ordering::Acquire) || generation != state.snapshot.generation {
                return Err(native::Error::Changed);
            }
            if format != state.format {
                return Err(native::Error::Unsupported);
            }
            let limits = native::Policy::shipped()?.limits;
            match format {
                Format::TextUtf8 => Ok(Value::Text(native::Text::new(state.bytes, &limits)?)),
                Format::ImagePng => Ok(Value::Image(super::super::image::Png::new(
                    state.bytes,
                    &limits,
                )?)),
                Format::Files => Err(native::Error::Unsupported),
            }
        })
    }
    fn publish(
        &self,
        _: u64,
        _: native::Publication,
        _: NativeLifetime,
    ) -> native::Operation<'_, Snapshot> {
        Box::pin(async { Err(native::Error::Unsupported) })
    }
}

#[tokio::test]
async fn dropping_an_unreceived_request_cancels_the_exact_source_read_before_a_body_exists() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            let pair = Pair::new(backend).await;
            let left = Memory::new("left", ShortcutModifier::Control);
            let right = Memory::new("right", ShortcutModifier::Command);
            right.paused.store(true, Ordering::Release);
            let a = pair.prepare(left.clone(), &right, Side::Initiator);
            let b = pair.prepare(right.clone(), &left, Side::Responder);
            let a = a.start().unwrap();
            let b = b.start().unwrap();
            let ah = a.handle();
            let bh = b.handle();
            let remote = offer(&ah, 0).await;
            let pending = ah
                .request(
                    remote.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap();
            right.entered.notified().await;
            drop(pending);
            right.finished.notified().await;
            assert!(!ah.closed());
            assert!(!bh.closed());
            right.paused.store(false, Ordering::Release);
            let value = ah
                .request(
                    remote.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(value.bytes(), &Bytes::from_static(b"right"));
            value.acknowledge(false).unwrap();
            assert_eq!(right.reads.load(Ordering::Relaxed), 2);
            drop(value);
            let _ = a.close().await;
            let _ = b.close().await;
            drop(ah);
            drop(bh);
            pair.close().await;
        }
    })
    .await
    .unwrap();
}

struct Pair {
    left: Connection,
    right: Connection,
    client: rds_net::Endpoint,
    server: rds_net::Endpoint,
}

#[tokio::test]
async fn missing_native_metadata_times_out_without_closing_the_desktop_connection() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            let pair = Pair::new(backend).await;
            let provider = Memory::new("local", ShortcutModifier::Control);
            provider.metadata_paused.store(true, Ordering::Release);
            let other = Memory::new("other", ShortcutModifier::Command);
            let mut prepared = pair.prepare(provider.clone(), &other, Side::Initiator);
            prepared.policy.startup_timeout_ms = 100;
            let mut peer = prepared.start().unwrap();
            let handle = peer.handle();
            let result = peer.task.take().unwrap().await.unwrap();
            assert!(
                matches!(result, Err(Error::Io(error)) if error.kind()==io::ErrorKind::TimedOut)
            );
            assert!(handle.closed());
            assert!(provider.closed.load(Ordering::Acquire));
            let (mut send, _recv) = pair.left.open_bi().await.unwrap();
            let (sent, received) = tokio::join!(
                async {
                    send.write_all(b"desktop remains available").await.unwrap();
                    send.finish().unwrap();
                },
                async {
                    let (_send, mut recv) = pair.right.accept_bi().await.unwrap();
                    let mut bytes = [0; 25];
                    recv.read_exact(&mut bytes).await.unwrap();
                    bytes
                }
            );
            assert_eq!(sent, ());
            assert_eq!(received, *b"desktop remains available");
            drop(handle);
            drop(peer);
            pair.close().await;
        }
    })
    .await
    .unwrap();
}
impl Pair {
    async fn new(backend: rds_net::Backend) -> Self {
        let config = || rds_net::EndpointConfig {
            backend,
            discovery: false,
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            ..Default::default()
        };
        let client = rds_net::bind_endpoint(config()).await.unwrap();
        let server = rds_net::bind_endpoint(config()).await.unwrap();
        let (left, right) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
            server.accept().await.unwrap().await
        });
        Self {
            left: left.unwrap(),
            right: right.unwrap(),
            client,
            server,
        }
    }
    fn prepare(&self, provider: Arc<Memory>, other: &Memory, side: Side) -> Prepared {
        Prepared::new(Setup {
            connection: if side == Side::Initiator {
                self.left.clone()
            } else {
                self.right.clone()
            },
            desktop: [71; 16],
            side,
            admitted: capabilities(provider.as_ref()).unwrap(),
            peer: capabilities(other).unwrap(),
            provider,
            policy: native::Policy::shipped().unwrap().peer,
            original: Arc::new(()),
        })
        .unwrap()
    }
    async fn close(self) {
        self.left.close(0u32.into(), b"fixture complete");
        self.right.close(0u32.into(), b"fixture complete");
        self.client.close().await;
        self.server.close().await;
    }
}
async fn offer(handle: &Handle, after: u64) -> Offer {
    let mut changes = handle.offers();
    loop {
        let offer = changes.borrow_and_update().clone();
        if let Some(offer) = offer.filter(|offer| offer.id > after) {
            return offer;
        }
        changes.changed().await.unwrap();
    }
}
fn image() -> Bytes {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[31, 70, 150, 255]).unwrap();
        writer.finish().unwrap();
    }
    Bytes::from(bytes)
}

#[tokio::test]
async fn real_symmetric_metadata_only_reads_requested_text_and_png_on_both_transports() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            let pair = Pair::new(backend).await;
            let left = Memory::new("left ✓", ShortcutModifier::Control);
            let right = Memory::new("right 🖥️", ShortcutModifier::Command);
            let a = pair.prepare(left.clone(), &right, Side::Initiator);
            let b = pair.prepare(right.clone(), &left, Side::Responder);
            let a = a.start().unwrap();
            let b = b.start().unwrap();
            let ah = a.handle();
            let bh = b.handle();
            let (ao, bo) = tokio::join!(offer(&ah, 0), offer(&bh, 0));
            assert_eq!(ah.peer_shortcut(), ShortcutModifier::Command);
            assert_eq!(bh.peer_shortcut(), ShortcutModifier::Control);
            assert_eq!(left.reads.load(Ordering::Relaxed), 0);
            assert_eq!(right.reads.load(Ordering::Relaxed), 0);
            let ar = ah
                .request(
                    ao.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap();
            let br = bh
                .request(
                    bo.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap();
            assert_ne!(ar.request().id, br.request().id);
            let (ar, br) = tokio::join!(ar.bytes(), br.bytes());
            let ar = ar.unwrap();
            let br = br.unwrap();
            assert_eq!(ar.bytes(), &Bytes::from_static("right 🖥️".as_bytes()));
            assert_eq!(br.bytes(), &Bytes::from_static("left ✓".as_bytes()));
            let ar = ar.publication().await.unwrap();
            let native::Publication::Text(text) = ar.value else {
                panic!("requested text must prepare as text");
            };
            assert_eq!(text.as_str(), "right 🖥️");
            assert_eq!(text.encoded().as_ptr(), ar.received.bytes().as_ptr());
            let ar = ar.received;
            assert!(ar.acknowledge(false).unwrap());
            assert!(!ar.acknowledge(false).unwrap());
            br.acknowledge(false).unwrap();
            drop(ar);
            drop(br);
            let png = image();
            right.change(Format::ImagePng, png.clone());
            let next = offer(&ah, ao.id).await;
            let incoming = ah
                .request(
                    next.id,
                    Format::ImagePng,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap();
            let prepared = incoming.bytes().await.unwrap().publication().await.unwrap();
            let native::Publication::Image(decoded) = prepared.value else {
                panic!("requested PNG must prepare as a validated image");
            };
            assert_eq!(decoded.encoded(), &png);
            assert_eq!(
                decoded.encoded().as_ptr(),
                prepared.received.bytes().as_ptr()
            );
            assert!(prepared.received.acknowledge(false).unwrap());
            drop(prepared.received);
            let canceled = CancellationToken::new();
            let received = ah
                .request(next.id, Format::ImagePng, canceled.clone(), Arc::new(()))
                .unwrap()
                .bytes()
                .await
                .unwrap();
            canceled.cancel();
            assert!(matches!(received.publication().await, Err(Error::Closed)));
            assert!(matches!(
                ah.request(
                    ao.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(())
                ),
                Err(Error::Contract(_))
            ));
            let _ = a.close().await;
            let _ = b.close().await;
            assert!(left.closed.load(Ordering::Acquire));
            assert!(right.closed.load(Ordering::Acquire));
            drop(ah);
            drop(bh);
            pair.close().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn opposite_request_namespaces_keep_publication_failure_out_of_the_reverse_body() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for backend in [rds_net::Backend::Iroh, rds_net::Backend::Noq] {
            let pair = Pair::new(backend).await;
            let left = Memory::new("left", ShortcutModifier::Control);
            let right = Memory::new("right", ShortcutModifier::Command);
            left.paused.store(true, Ordering::Release);
            let a = pair.prepare(left.clone(), &right, Side::Initiator);
            let b = pair.prepare(right.clone(), &left, Side::Responder);
            let a = a.start().unwrap();
            let b = b.start().unwrap();
            let ah = a.handle();
            let bh = b.handle();
            let (ao, bo) = tokio::join!(offer(&ah, 0), offer(&bh, 0));
            let incoming = bh
                .request(
                    bo.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap();
            let mut reverse = Box::pin(incoming.bytes());
            let first = ah
                .request(
                    ao.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap()
                .bytes()
                .await
                .unwrap();
            left.entered.notified().await;
            ah.0.send(Control::Failed {
                id: first.receipt().id,
                code: Failure::NativePublication,
            })
            .unwrap();
            // A later body proves that the preceding failure metadata crossed
            // the same ordered stream and was handled by the other actor.
            let second = ah
                .request(
                    ao.id,
                    Format::TextUtf8,
                    CancellationToken::new(),
                    Arc::new(()),
                )
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(second.bytes(), &Bytes::from_static(b"right"));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    reverse.as_mut().poll(cx).is_pending()
                ))
                .await,
                "publication failure crossed request directions"
            );
            left.release.notify_one();
            assert_eq!(reverse.await.unwrap().bytes(), &Bytes::from_static(b"left"));
            drop(first);
            drop(second);
            let _ = a.close().await;
            let _ = b.close().await;
            drop(ah);
            drop(bh);
            pair.close().await;
        }
    })
    .await
    .unwrap();
}
