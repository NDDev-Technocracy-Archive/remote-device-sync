use super::*;
use std::sync::atomic::AtomicUsize;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires dedicated Xvfb; owns its text clipboard"]
async fn native_close_waits_for_original_cleanup_before_returning_body_credit() {
    let policy = Policy::shipped().unwrap();
    let prepared = prepare(policy.clone(), Arc::new(())).await.unwrap();
    let provider = prepared.provider(Arc::new(())).unwrap();
    let (source, _, _) = Source::start(Bytes::from_static(b"native cleanup"), false, false).await;
    let current = provider.current().await.unwrap();
    let (original, mut cleanup) = crate::clipboard::ownership::probe::Cleanup::held();
    let Value::Text(value) = provider
        .read(current.generation, Format::TextUtf8, original)
        .await
        .unwrap()
    else {
        panic!("native read changed its representation");
    };
    assert_eq!(value.as_str(), "native cleanup");
    cleanup.entered().await.unwrap();
    let mut closing = provider.close();
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx).is_pending()))
            .await;
    let held = prepared.0.bodies.available_permits() == policy.native_body_operations - 1;
    cleanup.release().await.unwrap();
    if pending {
        closing.await;
    } else {
        drop(closing);
    }
    let returned = prepared.0.bodies.available_permits();
    drop(provider);
    source.close().await;
    prepared.close().await;
    // Stop the fixture's real native workers before reporting a baseline
    // failure; an assertion must not leave their blocking loops alive.
    assert!(pending, "native close acknowledged before original cleanup");
    assert!(held, "native body credit returned before original cleanup");
    assert_eq!(returned, policy.native_body_operations);
}

struct Source {
    wake: UnixStream,
    stopped: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
    bodies: Arc<AtomicUsize>,
}
impl Source {
    async fn start(
        bytes: Bytes,
        png: bool,
        block_body: bool,
    ) -> (Self, oneshot::Receiver<()>, queue::Sender<()>) {
        let (wake, mut receiver) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let (ready, initialized) = oneshot::channel();
        let (entered, started) = oneshot::channel();
        let (release, gate) = queue::channel();
        let bodies = Arc::new(AtomicUsize::new(0));
        let requests = bodies.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut board = Clipboard::new_watching(0, false, &Policy::shipped().unwrap()).unwrap();
            let target = if png {
                board
                    .conn
                    .intern_atom(false, b"image/png")
                    .unwrap()
                    .reply()
                    .unwrap()
                    .atom
            } else {
                board.utf8
            };
            board.publish_bytes(target, bytes, None).unwrap();
            let mut blocked = block_body.then_some((entered, gate));
            let _ = ready.send(());
            while !stop.load(Ordering::Acquire) {
                while let Some(event) = board.conn.poll_for_event().unwrap() {
                    if matches!(&event, Event::SelectionRequest(event) if event.target == target) {
                        requests.fetch_add(1, Ordering::Relaxed);
                    }
                    match event {
                        Event::SelectionRequest(event)
                            if event.target == target && blocked.is_some() =>
                        {
                            let (entered, gate) = blocked.take().unwrap();
                            let _ = entered.send(());
                            gate.recv_timeout(Duration::from_secs(10)).unwrap();
                            board.request(event).unwrap();
                        }
                        event => board.event(event).unwrap(),
                    }
                    board.conn.flush().unwrap();
                }
                let mut fds = [
                    rustix::event::PollFd::new(board.conn.stream(), rustix::event::PollFlags::IN),
                    rustix::event::PollFd::new(&receiver, rustix::event::PollFlags::IN),
                ];
                rustix::event::poll(&mut fds, None).unwrap();
                let mut buffer = [0; 64];
                while receiver.read(&mut buffer).is_ok_and(|n| n > 0) {}
            }
        });
        initialized.await.unwrap();
        (
            Self {
                wake,
                stopped,
                task: Some(task),
                bodies,
            },
            started,
            release,
        )
    }
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.write(&[1]);
    }
    async fn close(mut self) {
        self.stop();
        if let Some(task) = self.task.take() {
            task.await.unwrap();
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}

fn image(policy: &Policy) -> crate::clipboard::image::Png {
    let mut random = 19u32;
    let mut pixels = Vec::with_capacity(256 * 128 * 4);
    for _ in 0..256 * 128 * 4 {
        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
        pixels.push((random >> 24) as u8);
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 256, 128);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixels).unwrap();
        writer.finish().unwrap();
    }
    assert!(bytes.len() > 32 * 1024, "fixture must exercise native INCR");
    crate::clipboard::image::Png::new(Bytes::from(bytes), &policy.limits).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires dedicated Xvfb; owns its text and image clipboard"]
async fn native_x11_provider_reads_and_publishes_exact_text_png_and_ownership() {
    let policy = Policy::shipped().unwrap();
    let root = Arc::new(());
    let prepared = prepare(policy.clone(), root.clone()).await.unwrap();
    let purpose = Arc::new(());
    let retained = Arc::downgrade(&purpose);
    let provider = prepared.provider(purpose).unwrap();
    assert_eq!(provider.capabilities().shortcut, ShortcutModifier::Control);
    assert_eq!(
        provider.capabilities().offer,
        vec![Format::TextUtf8, Format::ImagePng]
    );
    let text = "Привет 🖥️\n".repeat(8000);
    let (source, _, _) = Source::start(Bytes::from(text.clone()), false, false).await;
    let snapshot = provider.current().await.unwrap();
    assert_eq!(source.bodies.load(Ordering::Relaxed), 0);
    assert!(!snapshot.owned);
    assert_eq!(
        snapshot.representations,
        vec![Representation {
            format: Format::TextUtf8,
            bytes: None
        }]
    );
    let Value::Text(value) = provider
        .read(snapshot.generation, Format::TextUtf8, Arc::new(()))
        .await
        .unwrap()
    else {
        panic!("native text changed representation");
    };
    assert!(value.as_str() == text, "native UTF-8 bytes changed");
    assert_eq!(source.bodies.load(Ordering::Relaxed), 1);
    source.close().await;
    let png = image(&policy);
    let expected = png.encoded().clone();
    let (source, _, _) = Source::start(expected.clone(), true, false).await;
    let snapshot = provider.changed(snapshot.generation).await.unwrap();
    assert_eq!(source.bodies.load(Ordering::Relaxed), 0);
    assert!(!snapshot.owned);
    let Value::Image(value) = provider
        .read(snapshot.generation, Format::ImagePng, Arc::new(()))
        .await
        .unwrap()
    else {
        panic!("native PNG changed representation");
    };
    assert!(value.encoded() == &expected, "native PNG bytes changed");
    let published = provider
        .publish(snapshot.generation, Publication::Image(png), Arc::new(()))
        .await
        .unwrap();
    assert!(published.owned);
    assert!(matches!(
        provider
            .publish(
                snapshot.generation,
                Publication::Text(Text::new(Bytes::from_static(b"stale"), &policy.limits).unwrap()),
                Arc::new(())
            )
            .await,
        Err(Error::Changed)
    ));
    let unchanged = provider.current().await.unwrap();
    assert_eq!(unchanged.generation, published.generation);
    let (observer, screen) = RustConnection::connect(None).unwrap();
    let window = observer.generate_id().unwrap();
    observer
        .create_window(
            0,
            window,
            observer.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
    let atom = |name: &[u8]| {
        observer
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let selection = atom(b"CLIPBOARD");
    let target = atom(b"image/png");
    let property = atom(b"NDDEV_NATIVE_CLIPBOARD_TEST");
    let received =
        super::super::tests::receive(&observer, window, selection, target, property).await;
    assert!(
        received.as_slice() == expected.as_ref(),
        "native PNG publication changed bytes"
    );
    observer.destroy_window(window).unwrap().check().unwrap();
    source.close().await;
    provider.close().await;
    assert!(matches!(provider.current().await, Err(Error::Changed)));
    assert!(retained.upgrade().is_some());
    drop(provider);
    assert!(retained.upgrade().is_none());
    prepared.close().await;
    assert!(prepared.provider(Arc::new(())).is_err());
    drop(prepared);
    drop(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires dedicated Xvfb; stalls a real native selection reply"]
async fn native_x11_canceled_read_keeps_its_body_and_original_owner_until_native_drain() {
    let mut policy = Policy::shipped().unwrap();
    policy.native_body_operations = 1;
    let root = Arc::new(());
    let prepared = prepare(policy.clone(), root.clone()).await.unwrap();
    let provider = prepared.provider(Arc::new(())).unwrap();
    let (source, entered, release) =
        Source::start(Bytes::from_static(b"delayed native text"), false, true).await;
    let snapshot = provider.current().await.unwrap();
    let original = Arc::new(());
    let retained = Arc::downgrade(&original);
    let native = provider.clone();
    let pending = tokio::spawn(async move {
        native
            .read(snapshot.generation, Format::TextUtf8, original)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    pending.abort();
    match pending.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("canceled native waiter unexpectedly returned"),
    }
    assert!(retained.upgrade().is_some());
    assert!(matches!(
        provider
            .read(snapshot.generation, Format::TextUtf8, Arc::new(()))
            .await,
        Err(Error::Unavailable)
    ));
    let mut closed = provider.close();
    tokio::select! {
        biased;
        _ = &mut closed => panic!("native close returned before the actual reply drained"),
        _ = tokio::task::yield_now() => {},
    }
    assert!(retained.upgrade().is_some());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .unwrap();
    assert!(retained.upgrade().is_none());
    source.close().await;
    drop(provider);
    prepared.close().await;
    drop(prepared);
    drop(root);
}
