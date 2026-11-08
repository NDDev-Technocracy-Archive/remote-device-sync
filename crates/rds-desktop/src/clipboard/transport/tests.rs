use super::*;
use rds_core::clipboard::Format;
use tokio::io::AsyncReadExt;

struct Generated {
    selection: rds_core::clipboard::FileSelection,
    limits: Limits,
    byte: u8,
}
impl Generated {
    fn new(byte: u8) -> Self {
        Self {
            selection: rds_core::clipboard::FileSelection {
                entries: vec![rds_core::clipboard::FileEntry {
                    id: [7; 16],
                    name: "generated.bin".into(),
                    bytes: 64 * 1024 * 1024,
                }],
            },
            limits: Limits::default(),
            byte,
        }
    }
}
impl native::Files for Generated {
    fn selection(&self) -> &rds_core::clipboard::FileSelection {
        &self.selection
    }
    fn limits(&self) -> &Limits {
        &self.limits
    }
    fn open(
        &self,
        file: [u8; 16],
        original: NativeLifetime,
    ) -> native::Operation<'_, native::FileRead> {
        Box::pin(async move {
            assert_eq!(file, [7; 16]);
            Ok(native::FileRead {
                bytes: self.selection.entries[0].bytes,
                reader: Box::pin(
                    tokio::io::repeat(self.byte).take(self.selection.entries[0].bytes),
                ),
                complete: Box::pin(async move {
                    drop(original);
                    Ok(())
                }),
            })
        })
    }
}

struct Pair {
    left: Connection,
    right: Connection,
    _a: rds_net::Endpoint,
    _b: rds_net::Endpoint,
}
impl Pair {
    async fn new() -> Self {
        Self::backend(rds_net::Backend::Iroh).await
    }
    async fn backend(backend: rds_net::Backend) -> Self {
        let config = rds_net::EndpointConfig {
            backend,
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            discovery: false,
            ..Default::default()
        };
        let a = rds_net::bind_endpoint(config.clone()).await.unwrap();
        let b = rds_net::bind_endpoint(config).await.unwrap();
        let (left, right) = tokio::join!(a.connect(b.addr(), rds_core::ALPN), async {
            b.accept().await.unwrap().await
        });
        Self {
            left: left.unwrap(),
            right: right.unwrap(),
            _a: a,
            _b: b,
        }
    }
    fn owners(&self) -> (Streams, Streams) {
        (
            Streams::new(self.left.clone(), [19; 16], Limits::default(), Arc::new(())).unwrap(),
            Streams::new(
                self.right.clone(),
                [19; 16],
                Limits::default(),
                Arc::new(()),
            )
            .unwrap(),
        )
    }
}

#[tokio::test]
async fn symmetric_budgets_and_exact_routes_keep_canceled_bodies_out_of_new_requests() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pair = Pair::new().await;
        let (left, right) = pair.owners();
        let old = left
            .request(
                1,
                PayloadKind::Representation(Format::TextUtf8),
                None,
                CancellationToken::new(),
            )
            .unwrap();
        let old_request = old.request().clone();
        let next = left
            .request(
                2,
                PayloadKind::Representation(Format::TextUtf8),
                None,
                CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(
            left.request(
                3,
                PayloadKind::Representation(Format::TextUtf8),
                None,
                CancellationToken::new()
            ),
            Err(Error::Busy)
        ));
        let reverse = right
            .request(
                1,
                PayloadKind::Representation(Format::ImagePng),
                Some(4),
                CancellationToken::new(),
            )
            .unwrap();
        let next_request = next.request().clone();
        let reverse_request = reverse.request().clone();
        drop(old);
        assert_ne!(old_request.id, next_request.id);
        // A late response has no registered inbox, even when its replacement
        // is actively receiving. It can never supply that replacement's data.
        let _ = right
            .send_bytes(&old_request, Bytes::from_static(b"old"))
            .await;
        let (sent, received, reverse_sent, reverse_received) = tokio::join!(
            right.send_bytes(&next_request, Bytes::from_static(b"fresh")),
            next.bytes(),
            left.send_bytes(&reverse_request, Bytes::from_static(b"png!")),
            reverse.bytes(),
        );
        let (value, receipt) = received.unwrap();
        assert_eq!(value, b"fresh"[..]);
        assert_eq!(receipt, sent.unwrap());
        let (value, receipt) = reverse_received.unwrap();
        assert_eq!(value, b"png!"[..]);
        assert_eq!(receipt, reverse_sent.unwrap());
        assert!(!receipt.published);
        assert_eq!(pair.left.uni_routing_stats().routes, 0);
        assert_eq!(pair.right.uni_routing_stats().routes, 0);
        left.close();
        assert!(matches!(
            left.request(
                4,
                PayloadKind::Representation(Format::TextUtf8),
                None,
                CancellationToken::new()
            ),
            Err(Error::Closed)
        ));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn real_selected_file_crosses_quic_without_a_staging_file() {
    let root = std::env::temp_dir().join(format!(
        "rds-clipboard-quic-{:032x}",
        rand::random::<u128>()
    ));
    std::fs::create_dir(&root).unwrap();
    let value = "native clipboard 🖥️\n".repeat(20_000).into_bytes();
    std::fs::write(root.join("selected"), &value).unwrap();
    let selected =
        rds_sync::selection::Selection::from_paths([root.join("selected")], &Limits::default())
            .unwrap();
    let file = selected.manifest().entries[0].id;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pair = Pair::new().await;
        let (left, right) = pair.owners();
        let incoming = left
            .request(
                1,
                PayloadKind::File(file),
                Some(value.len() as u64),
                CancellationToken::new(),
            )
            .unwrap();
        let request = incoming.request().clone();
        let (sent, received) = tokio::join!(right.send_file(&request, &selected), async {
            let mut received = incoming.file().await.unwrap();
            let mut bytes = Vec::new();
            received.reader.read_to_end(&mut bytes).await.unwrap();
            received.complete.await.unwrap();
            bytes
        });
        assert_eq!(sent.unwrap().bytes, value.len() as u64);
        assert_eq!(received, value);
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn remote_native_promise_writes_only_its_selected_file_and_receipts_verified_bytes() {
    use native::Files;
    use rds_core::clipboard::Control;
    let root = std::env::temp_dir().join(format!(
        "rds-remote-promise-{:032x}",
        rand::random::<u128>()
    ));
    std::fs::create_dir(&root).unwrap();
    let bytes = "remote file ✓\n".repeat(30_000).into_bytes();
    std::fs::write(root.join("selected"), &bytes).unwrap();
    let selected =
        rds_sync::selection::Selection::from_paths([root.join("selected")], &Limits::default())
            .unwrap();
    let file = selected.manifest().entries[0].id;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pair = Pair::backend(rds_net::Backend::Noq).await;
        let (left, right) = pair.owners();
        let (metadata, mut requests) = tokio::sync::mpsc::channel(4);
        let scope = CancellationToken::new();
        let remote = native::RemoteFiles::new(5, selected.manifest().clone(), Arc::new(left), metadata, scope.clone()).unwrap();
        assert!(matches!(remote.open([0;16], Arc::new(())).await, Err(native::Error::File(_))));
        assert!(requests.try_recv().is_err());
        let destination = std::fs::OpenOptions::new().write(true).create_new(true).open(root.join("pasted")).unwrap();
        let (sent, received) = tokio::join!(
            async {
                let Control::Request(request) = requests.recv().await.unwrap() else { panic!("missing request") };
                assert_eq!(request.kind, PayloadKind::File(file));
                let receipt = right.send_file(&request, &selected).await.unwrap();
                assert!(matches!(requests.recv().await.unwrap(), Control::Receipt(verified) if verified==receipt && !verified.published));
                receipt
            },
            remote.write(file, native::Destination::new(destination, Arc::new(()))),
        );
        let received = received.unwrap();
        assert_eq!(received.bytes, sent.bytes);
        assert_eq!(received.digest, sent.digest);
        scope.cancel();
        assert!(matches!(remote.open(file, Arc::new(())).await, Err(native::Error::Changed)));
        assert!(requests.try_recv().is_err());
    }).await.unwrap();
    assert_eq!(std::fs::read(root.join("pasted")).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn outgoing_reservation_survives_cancellation_of_its_actual_native_open() {
    struct Gate {
        entered: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    struct NativeOpening {
        selected: rds_sync::selection::Selection,
        gate: std::sync::Mutex<Option<Gate>>,
    }
    impl native::Files for NativeOpening {
        fn selection(&self) -> &rds_core::clipboard::FileSelection {
            self.selected.manifest()
        }
        fn limits(&self) -> &Limits {
            self.selected.limits()
        }
        fn open(
            &self,
            file: [u8; 16],
            original: NativeLifetime,
        ) -> native::Operation<'_, native::FileRead> {
            Box::pin(async move {
                let file = self.selected.file(&file)?;
                let gate = self.gate.lock().unwrap().take().unwrap();
                tokio::task::spawn_blocking(move || {
                    let owners = (file, original);
                    let held = owners;
                    held.0.verify()?;
                    gate.entered.send(()).unwrap();
                    gate.release
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap();
                    assert_eq!(held.0.read_chunk(0, 8, &Limits::default())?, b"selected");
                    Ok::<_, native::Error>(())
                })
                .await
                .map_err(|_| native::Error::Unavailable)??;
                Err(native::Error::Changed)
            })
        }
    }
    let root =
        std::env::temp_dir().join(format!("rds-native-body-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("selected"), b"selected").unwrap();
    let selected =
        rds_sync::selection::Selection::from_paths([root.join("selected")], &Limits::default())
            .unwrap();
    let file = selected.manifest().entries[0].id;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let source = NativeOpening {
        selected,
        gate: std::sync::Mutex::new(Some(Gate {
            entered,
            release: held,
        })),
    };
    let pair = Pair::new().await;
    let (left, _) = pair.owners();
    let right = Arc::new(
        Streams::new(
            pair.right.clone(),
            [19; 16],
            Limits {
                parallel_transfers: 1,
                ..Limits::default()
            },
            Arc::new(()),
        )
        .unwrap(),
    );
    let incoming = left
        .request(
            1,
            PayloadKind::File(file),
            Some(8),
            CancellationToken::new(),
        )
        .unwrap();
    let request = incoming.request().clone();
    let sender = right.clone();
    let sending = tokio::spawn(async move { sender.send_file(&request, &source).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    sending.abort();
    assert!(sending.await.unwrap_err().is_cancelled());
    assert!(
        right.outgoing.try_acquire().is_err(),
        "canceled sender released a running native body"
    );
    release.send(()).unwrap();
    let _drained =
        tokio::time::timeout(std::time::Duration::from_secs(2), right.outgoing.acquire())
            .await
            .unwrap()
            .unwrap();
    drop(incoming);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn revoking_a_remote_file_scope_stops_an_existing_reader_and_cancels_its_request() {
    use native::Files;
    use rds_core::clipboard::{Control, FileEntry, FileSelection};
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pair = Pair::new().await;
        let (left, right) = pair.owners();
        let right = Arc::new(right);
        let (metadata, mut requests) = tokio::sync::mpsc::channel(4);
        let scope = CancellationToken::new();
        let remote = native::RemoteFiles::new(
            6,
            FileSelection {
                entries: vec![FileEntry {
                    id: [7; 16],
                    name: "selected.bin".into(),
                    bytes: 64 * 1024 * 1024,
                }],
            },
            Arc::new(left),
            metadata,
            scope.clone(),
        )
        .unwrap();
        let open = remote.open([7; 16], Arc::new(()));
        let mut sending = None;
        let sender = right.clone();
        let (read, request) = tokio::join!(open, async {
            let Control::Request(request) = requests.recv().await.unwrap() else {
                panic!("missing request")
            };
            let sending_request = request.clone();
            sending = Some(tokio::spawn(async move {
                sender.send_file(&sending_request, &Generated::new(8)).await
            }));
            request
        });
        let mut read = read.unwrap();
        scope.cancel();
        assert_eq!(
            read.reader.read(&mut [0; 1]).await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        drop(read.reader);
        assert!(read.complete.await.is_err());
        assert!(matches!(requests.recv().await.unwrap(), Control::Cancel {id} if id==request.id));
        right.close();
        assert!(sending.unwrap().await.unwrap().is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn blocked_bulk_preserves_control_media_and_cancellation_ownership() {
    blocked_bulk(rds_net::Backend::Iroh).await;
}

#[tokio::test]
async fn owned_transport_preserves_control_media_and_cancellation_ownership() {
    blocked_bulk(rds_net::Backend::Noq).await;
}

async fn blocked_bulk(backend: rds_net::Backend) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pair = Pair::backend(backend).await;
        let original = Arc::new(());
        let retained = Arc::downgrade(&original);
        let left = Streams::new(pair.left.clone(), [19; 16], Limits::default(), original).unwrap();
        let right = Arc::new(
            Streams::new(
                pair.right.clone(),
                [19; 16],
                Limits::default(),
                Arc::new(()),
            )
            .unwrap(),
        );
        let incoming = left
            .request(
                1,
                PayloadKind::File([7; 16]),
                Some(64 * 1024 * 1024),
                CancellationToken::new(),
            )
            .unwrap();
        let request = incoming.request().clone();
        let sender = right.clone();
        let sending =
            tokio::spawn(async move { sender.send_file(&request, &Generated::new(7)).await });
        let mut body = incoming.file().await.unwrap();
        // The receiver holds the file capability without consuming its body.
        // Reliable control and media still progress on this same connection.
        let mut media = pair
            .left
            .uni_streams(UniHello::DesktopFrames { id: [22; 16] })
            .unwrap();
        let (mut control_send, _) = pair.right.open_bi().await.unwrap();
        control_send
            .set_priority(rds_net::wire::CONTROL_STREAM_PRIORITY)
            .unwrap();
        rds_net::write_frame(&mut control_send, &1234u64)
            .await
            .unwrap();
        let (_, mut control_receive) = pair.left.accept_bi().await.unwrap();
        assert_eq!(
            rds_net::read_frame::<_, u64>(&mut control_receive)
                .await
                .unwrap(),
            1234
        );
        let mut frame = pair.right.open_uni().await.unwrap();
        frame
            .set_priority(rds_net::wire::MEDIA_STREAM_PRIORITY)
            .unwrap();
        rds_net::write_frame(&mut frame, &UniHello::DesktopFrames { id: [22; 16] })
            .await
            .unwrap();
        frame.write_all(b"media").await.unwrap();
        frame.finish().unwrap();
        let mut frame = media.recv().await.unwrap();
        let bytes = frame.read_to_end(5).await.unwrap();
        assert_eq!(bytes, b"media");
        assert!(!sending.is_finished());
        right.close();
        assert!(matches!(sending.await.unwrap(), Err(Error::Closed)));
        left.close();
        drop(left);
        assert!(retained.upgrade().is_some());
        assert_eq!(
            body.reader.read(&mut [0; 1]).await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        drop(body.reader);
        assert!(
            retained.upgrade().is_some(),
            "integrity proof also owns its original caller"
        );
        assert!(body.complete.await.is_err());
        assert!(retained.upgrade().is_none());
    })
    .await
    .unwrap();
}
