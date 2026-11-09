use super::*;

fn guard(original: NativeLifetime) -> Guard {
    Guard {
        running: None,
        _body: Arc::new(Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()),
        _original: original,
    }
}

#[tokio::test]
async fn scoped_native_pipe_validates_utf8_limits_and_closes_before_its_original() {
    let policy = Policy::shipped().unwrap();
    let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
    let original = Arc::new(());
    let weak = Arc::downgrade(&original);
    let io = NativeIo::new(read.into(), guard(original), Direction::Read).unwrap();
    let text = Text::new(
        Bytes::from_static("Привет\nclipboard ✓".as_bytes()),
        &policy.limits,
    )
    .unwrap();
    let output = NativeIo::new(write.into(), guard(Arc::new(())), Direction::Write).unwrap();
    let writing = async {
        output
            .write(&Body::Text(text.clone()), &policy)
            .await
            .unwrap();
        drop(output);
    };
    let (bytes, ()) = tokio::join!(io.read(policy.limits.text_bytes as usize, &policy), writing);
    assert_eq!(bytes.unwrap(), *text.encoded());
    assert!(weak.upgrade().is_some());
    drop(io);
    assert!(weak.upgrade().is_none());

    let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
    let io = NativeIo::new(read.into(), guard(Arc::new(())), Direction::Read).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), io.read(8, &policy))
            .await
            .is_err()
    );
    drop(io);
    assert_eq!(
        rustix::io::write(&write, b"x"),
        Err(rustix::io::Errno::PIPE)
    );
}

#[tokio::test]
async fn native_pipe_refuses_an_oversized_body_without_a_staging_copy() {
    let policy = Policy::shipped().unwrap();
    let (read, write) = std::os::unix::net::UnixStream::pair().unwrap();
    rustix::io::write(&write, b"too much").unwrap();
    drop(write);
    let io = NativeIo::new(read.into(), guard(Arc::new(())), Direction::Read).unwrap();
    assert!(matches!(
        io.read(3, &policy).await,
        Err(Error::Contract(rds_core::clipboard::Error::TooLarge))
    ));
}

#[tokio::test]
async fn actual_pipe_uses_safe_directional_registration_and_drops_fd_before_authority() {
    let policy = Policy::shipped().unwrap();
    let (sender, receiver) = pipe::pipe().unwrap();
    let read = NativeIo::new(
        receiver.into_nonblocking_fd().unwrap(),
        guard(Arc::new(())),
        Direction::Read,
    )
    .unwrap();
    let write = NativeIo::new(
        sender.into_nonblocking_fd().unwrap(),
        guard(Arc::new(())),
        Direction::Write,
    )
    .unwrap();
    let text = Text::new(
        Bytes::from("native pipe ✓".repeat(policy.portal.selection_chunk_bytes as usize)),
        &policy.limits,
    )
    .unwrap();
    let expected = text.encoded().clone();
    let writing = async {
        write.write(&Body::Text(text), &policy).await.unwrap();
        drop(write);
    };
    let (value, ()) = tokio::join!(
        read.read(policy.limits.text_bytes as usize, &policy),
        writing
    );
    assert_eq!(value.unwrap(), expected);
    drop(read);

    struct Original {
        peer: OwnedFd,
        closed: Arc<AtomicBool>,
    }
    impl Drop for Original {
        fn drop(&mut self) {
            assert_eq!(
                rustix::io::write(&self.peer, b"x"),
                Err(rustix::io::Errno::PIPE)
            );
            self.closed.store(true, Ordering::Release);
        }
    }
    let (sender, receiver) = pipe::pipe().unwrap();
    let closed = Arc::new(AtomicBool::new(false));
    let io = NativeIo::new(
        receiver.into_nonblocking_fd().unwrap(),
        guard(Arc::new(Original {
            peer: sender.into_nonblocking_fd().unwrap(),
            closed: closed.clone(),
        })),
        Direction::Read,
    )
    .unwrap();
    assert!(!closed.load(Ordering::Acquire));
    drop(io);
    assert!(closed.load(Ordering::Acquire));

    let (sender, receiver) = pipe::pipe().unwrap();
    assert!(
        NativeIo::new(
            receiver.into_nonblocking_fd().unwrap(),
            guard(Arc::new(Original {
                peer: sender.into_nonblocking_fd().unwrap(),
                closed: Arc::new(AtomicBool::new(false)),
            })),
            Direction::Write,
        )
        .is_err()
    );
}

#[test]
fn a_replacement_portal_owner_cannot_reset_outstanding_native_body_credit() {
    let pool = Bodies::default();
    let first = pool.get(1).unwrap();
    let permit = first.try_acquire_owned().unwrap();
    let replacement = pool.get(1).unwrap();
    assert!(replacement.try_acquire().is_err());
    assert!(matches!(pool.get(2), Err(Error::Settings)));
    drop(replacement);
    drop(permit);
    assert_eq!(pool.get(2).unwrap().available_permits(), 2);
}

use ashpd::zbus::{self, OwnedFd as BusFd, OwnedVariant, Variant};
use std::{collections::HashMap, sync::atomic::AtomicUsize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Notify,
};

const OBJECT: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Clipboard";

fn owner_options(types: Vec<String>, owned: bool) -> HashMap<&'static str, Variant<'static>> {
    HashMap::from([
        ("mime_types", Variant::from(types)),
        ("session_is_owner", Variant::from(owned)),
    ])
}
struct RemoteFixture;
#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop", crate = "ashpd::zbus")]
impl RemoteFixture {
    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
    async fn create_session(
        &self,
        options: HashMap<String, OwnedVariant>,
        #[zbus(connection)] connection: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header
            .sender()
            .unwrap()
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let request =
            OwnedObjectPath::try_from(format!("{OBJECT}/request/{sender}/{token}")).unwrap();
        let token = <&str>::try_from(options.get("session_handle_token").unwrap()).unwrap();
        let session =
            OwnedObjectPath::try_from(format!("{OBJECT}/session/{sender}/{token}")).unwrap();
        connection
            .emit_signal(
                header.sender().cloned(),
                request.clone(),
                "org.freedesktop.portal.Request",
                "Response",
                &(
                    0_u32,
                    HashMap::from([("session_handle", OwnedVariant::from(session.into_inner()))]),
                ),
            )
            .await?;
        Ok(request)
    }
}
struct Data {
    source: Mutex<Bytes>,
    reads: AtomicUsize,
    publications: AtomicUsize,
    blocked: AtomicBool,
    entered: Notify,
    release: Semaphore,
    readers: Mutex<HashMap<u32, std::os::unix::net::UnixStream>>,
    done: Mutex<HashMap<u32, bool>>,
    completed: Notify,
    writers: Mutex<Vec<tokio::task::JoinHandle<std::io::Result<()>>>>,
}
impl Data {
    fn new() -> Self {
        Self {
            source: Mutex::new(Bytes::new()),
            reads: AtomicUsize::new(0),
            publications: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            release: Semaphore::new(0),
            readers: Mutex::new(HashMap::new()),
            done: Mutex::new(HashMap::new()),
            completed: Notify::new(),
            writers: Mutex::new(Vec::new()),
        }
    }
    async fn received(&self, serial: u32) -> Bytes {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let wake = self.completed.notified();
                if let Some(done) = self.done.lock().unwrap().get(&serial).copied() {
                    assert!(done);
                    break;
                }
                wake.await;
            }
        })
        .await
        .unwrap();
        let reader = self.readers.lock().unwrap().remove(&serial).unwrap();
        let mut reader = UnixStream::from_std(reader).unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        Bytes::from(bytes)
    }
}
struct ClipboardFixture(Arc<Data>);
#[zbus::interface(name = "org.freedesktop.portal.Clipboard", crate = "ashpd::zbus")]
impl ClipboardFixture {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
    async fn set_selection(
        &self,
        session: OwnedObjectPath,
        mut options: HashMap<String, OwnedVariant>,
        #[zbus(connection)] connection: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let types = Vec::<String>::try_from(options.remove("mime_types").unwrap()).unwrap();
        self.0.publications.fetch_add(1, Ordering::Relaxed);
        // Emit before the method reply, with no helper task or pacing delay.
        connection
            .emit_signal(
                header.sender().cloned(),
                OBJECT,
                INTERFACE,
                "SelectionOwnerChanged",
                &(session, owner_options(types, true)),
            )
            .await?;
        Ok(())
    }
    async fn selection_read(
        &self,
        _session: OwnedObjectPath,
        _mime: &str,
    ) -> zbus::fdo::Result<BusFd> {
        self.0.reads.fetch_add(1, Ordering::Relaxed);
        if self.0.blocked.load(Ordering::Acquire) {
            self.0.entered.notify_one();
            self.0.release.acquire().await.unwrap().forget();
        }
        let value = self.0.source.lock().unwrap().clone();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        self.0
            .writers
            .lock()
            .unwrap()
            .push(tokio::spawn(async move { writer.write_all(&value).await }));
        let fd: OwnedFd = reader.into_std().unwrap().into();
        Ok(fd.into())
    }
    async fn selection_write(
        &self,
        _session: OwnedObjectPath,
        serial: u32,
    ) -> zbus::fdo::Result<BusFd> {
        let (reader, writer) = UnixStream::pair().unwrap();
        self.0
            .readers
            .lock()
            .unwrap()
            .insert(serial, reader.into_std().unwrap());
        let fd: OwnedFd = writer.into_std().unwrap().into();
        Ok(fd.into())
    }
    async fn selection_write_done(
        &self,
        _session: OwnedObjectPath,
        serial: u32,
        success: bool,
    ) -> zbus::fdo::Result<()> {
        self.0.done.lock().unwrap().insert(serial, success);
        self.0.completed.notify_one();
        Ok(())
    }
}
async fn emit_owner(
    server: &zbus::Connection,
    client: &zbus::Connection,
    path: &OwnedObjectPath,
    types: &[&str],
) {
    server
        .emit_signal(
            client.unique_name().cloned(),
            OBJECT,
            INTERFACE,
            "SelectionOwnerChanged",
            &(
                path,
                owner_options(types.iter().map(|s| s.to_string()).collect(), false),
            ),
        )
        .await
        .unwrap();
}
async fn changed(provider: &dyn Provider, after: u64) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(3), provider.changed(after))
        .await
        .unwrap()
        .unwrap()
}
fn png() -> Bytes {
    use image::ImageEncoder;
    let mut pixels = vec![0; 64 * 64 * 4];
    let mut state = 0x4a_713d_u64;
    for pixel in &mut pixels {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *pixel = state as u8;
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .unwrap();
    Bytes::from(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portal_native_metadata_text_png_publication_and_cancelled_calls_share_one_scoped_owner() {
    let mut bus = tokio::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(bus.stdout.take().unwrap()).lines();
    let address = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let data = Arc::new(Data::new());
    let server = zbus::connection::Builder::address(address.as_str())
        .name("org.freedesktop.portal.Desktop")
        .serve_at(OBJECT, RemoteFixture)
        .serve_at(OBJECT, ClipboardFixture(data.clone()))
        .build()
        .await
        .unwrap();
    let client = zbus::connection::Builder::address(address.as_str())
        .build()
        .await
        .unwrap();
    let remote = RemoteDesktop::with_connection(client.clone())
        .await
        .unwrap();
    let session = Arc::new(remote.create_session(Default::default()).await.unwrap());
    let path = OwnedObjectPath::try_from(
        serde_json::to_value(session.as_ref())
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let proxy = Clipboard::with_connection(client.clone()).await.unwrap();
    let mut policy = Policy::shipped().unwrap();
    policy.operation_timeout_ms = 10_000;
    let group = Group::prepare(proxy, session.clone(), policy.clone())
        .await
        .unwrap();
    assert!(!group.available());
    assert!(group.native(Arc::new(())).is_err());
    // Model the portal's initial owner signal during Start, before activation.
    let text = Bytes::from("Привет clipboard ✓\n".repeat(3000));
    *data.source.lock().unwrap() = text.clone();
    emit_owner(&server, &client, &path, &[TEXT, PNG]).await;
    group.activate().await.unwrap();
    let original = Arc::new(());
    let lease_original = Arc::downgrade(&original);
    let provider = group.native(original).unwrap();
    let snapshot = changed(provider.as_ref(), 0).await;
    assert!(!snapshot.owned);
    assert_eq!(snapshot.representations.len(), 2);
    assert_eq!(data.reads.load(Ordering::Relaxed), 0);
    let caps = provider.capabilities();
    assert_eq!(caps.shortcut, ShortcutModifier::Control);
    assert_eq!(caps.receive, vec![Format::TextUtf8, Format::ImagePng]);
    assert!(!caps.offer.contains(&Format::Files));
    let value = provider
        .read(snapshot.generation, Format::TextUtf8, Arc::new(()))
        .await
        .unwrap();
    let Value::Text(value) = value else {
        panic!("expected UTF-8");
    };
    assert_eq!(value.encoded(), &text);
    assert_eq!(data.reads.load(Ordering::Relaxed), 1);

    let image = png();
    assert!(image.len() > policy.portal.selection_chunk_bytes as usize);
    *data.source.lock().unwrap() = image.clone();
    emit_owner(&server, &client, &path, &[PNG]).await;
    let next = changed(provider.as_ref(), snapshot.generation).await;
    let stale = Text::new(Bytes::from_static(b"stale"), &policy.limits).unwrap();
    assert!(matches!(
        provider
            .publish(snapshot.generation, Publication::Text(stale), Arc::new(()))
            .await,
        Err(Error::Changed)
    ));
    assert_eq!(data.publications.load(Ordering::Relaxed), 0);
    let value = provider
        .read(next.generation, Format::ImagePng, Arc::new(()))
        .await
        .unwrap();
    let Value::Image(value) = value else {
        panic!("expected PNG");
    };
    assert_eq!(value.encoded(), &image);
    let published = provider
        .publish(next.generation, Publication::Image(value), Arc::new(()))
        .await
        .unwrap();
    assert!(published.owned);
    assert_eq!(published.representations[0].bytes, Some(image.len() as u64));
    // A queued native read follows the earlier directed owner signal; an own
    // publication must not echo another generation or steal its request.
    let value = provider
        .read(published.generation, Format::ImagePng, Arc::new(()))
        .await
        .unwrap();
    let Value::Image(value) = value else {
        panic!("expected owned PNG");
    };
    assert_eq!(value.encoded(), &image);
    assert_eq!(
        provider.current().await.unwrap().generation,
        published.generation
    );
    let serial = 71_u32;
    server
        .emit_signal(
            client.unique_name().cloned(),
            OBJECT,
            INTERFACE,
            "SelectionTransfer",
            &(&path, PNG, serial),
        )
        .await
        .unwrap();
    assert_eq!(data.received(serial).await, image);

    *data.source.lock().unwrap() = text;
    emit_owner(&server, &client, &path, &[TEXT]).await;
    let external = changed(provider.as_ref(), published.generation).await;
    data.blocked.store(true, Ordering::Release);
    let original = Arc::new(());
    let native_original = Arc::downgrade(&original);
    let reading = provider.clone();
    let first = tokio::spawn(async move {
        reading
            .read(external.generation, Format::TextUtf8, original)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), data.entered.notified())
        .await
        .unwrap();
    first.abort();
    match first.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the canceled native reader completed normally"),
    }
    assert!(native_original.upgrade().is_some());
    let original = Arc::new(());
    let queued_original = Arc::downgrade(&original);
    let mut queued = provider.read(external.generation, Format::TextUtf8, original);
    assert!(matches!(
        futures_util::poll!(queued.as_mut()),
        std::task::Poll::Pending
    ));
    drop(queued);
    assert!(queued_original.upgrade().is_some());
    assert!(matches!(
        provider
            .read(external.generation, Format::TextUtf8, Arc::new(()))
            .await,
        Err(Error::Unavailable)
    ));
    let mut closing = provider.close();
    assert!(matches!(
        futures_util::poll!(closing.as_mut()),
        std::task::Poll::Pending
    ));
    assert!(lease_original.upgrade().is_some());
    assert!(native_original.upgrade().is_some());
    data.blocked.store(false, Ordering::Release);
    data.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), closing)
        .await
        .unwrap();
    assert!(native_original.upgrade().is_none());
    assert!(queued_original.upgrade().is_none());
    // A closed but still held provider keeps its original binding. Per-call
    // purposes above must already be released at the actual drain boundary.
    assert!(lease_original.upgrade().is_some());
    assert_eq!(
        group.0.bodies.available_permits(),
        policy.native_body_operations
    );
    assert!(matches!(provider.current().await, Err(Error::Changed)));
    assert_eq!(data.reads.load(Ordering::Relaxed), 3);
    let fresh = group.native(Arc::new(())).unwrap();
    assert_eq!(
        fresh.current().await.unwrap().generation,
        external.generation
    );
    let mut legacy = group.worker(false).unwrap();
    assert!(legacy.retain(Arc::new(())));
    legacy.publish("legacy paste".into()).await.unwrap();
    let owned = fresh.current().await.unwrap();
    let value = fresh
        .read(owned.generation, Format::TextUtf8, Arc::new(()))
        .await
        .unwrap();
    let Value::Text(value) = value else {
        panic!("expected legacy publication");
    };
    assert_eq!(value.as_str(), "legacy paste");
    legacy.close().await;
    let mut watching = group.worker(true).unwrap();
    assert!(watching.retain(Arc::new(())));
    data.blocked.store(true, Ordering::Release);
    let mut change = watching.next_change();
    assert!(matches!(
        futures_util::poll!(change.as_mut()),
        std::task::Poll::Pending
    ));
    emit_owner(&server, &client, &path, &[TEXT]).await;
    tokio::select! {
        _ = data.entered.notified() => {},
        _ = &mut change => panic!("legacy observation completed while native read was blocked"),
        _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("legacy observation did not start its native read"),
    }
    // The serving control select cancels next_change when input wins. Resume
    // that same observation without another owner event or a second read.
    drop(change);
    data.blocked.store(false, Ordering::Release);
    data.release.add_permits(1);
    let received = tokio::time::timeout(Duration::from_secs(3), watching.next_change())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.as_bytes(), data.source.lock().unwrap().as_ref());
    assert_eq!(data.reads.load(Ordering::Relaxed), 4);
    data.blocked.store(true, Ordering::Release);
    let mut superseded = watching.next_change();
    assert!(matches!(
        futures_util::poll!(superseded.as_mut()),
        std::task::Poll::Pending
    ));
    emit_owner(&server, &client, &path, &[TEXT]).await;
    tokio::select! {
        _ = data.entered.notified() => {},
        _ = &mut superseded => panic!("superseded observation completed before its native read"),
        _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("superseded observation did not start"),
    }
    drop(superseded);
    data.blocked.store(false, Ordering::Release);
    data.release.add_permits(1);
    // Admission of the entire physical body allowance proves the preceding
    // native read finished; its undelivered result is still in the adapter.
    let idle = tokio::time::timeout(
        Duration::from_secs(3),
        group
            .0
            .bodies
            .clone()
            .acquire_many_owned(policy.native_body_operations as u32),
    )
    .await
    .unwrap()
    .unwrap();
    drop(idle);
    watching
        .publish("new explicit publication wins".into())
        .await
        .unwrap();
    let owned = fresh.current().await.unwrap();
    let value = fresh
        .read(owned.generation, Format::TextUtf8, Arc::new(()))
        .await
        .unwrap();
    let Value::Text(value) = value else {
        panic!("expected newest publication");
    };
    assert_eq!(value.as_str(), "new explicit publication wins");
    let mut current = watching.next_change();
    assert!(
        matches!(
            futures_util::poll!(current.as_mut()),
            std::task::Poll::Pending
        ),
        "an old undelivered read must not override a later publication"
    );
    emit_owner(&server, &client, &path, &[TEXT]).await;
    let received = tokio::time::timeout(Duration::from_secs(3), current)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.as_bytes(), data.source.lock().unwrap().as_ref());
    assert_eq!(data.reads.load(Ordering::Relaxed), 6);
    watching.close().await;
    fresh.close().await;
    group.close().await;
    assert!(group.native(Arc::new(())).is_err());
    let writers = std::mem::take(&mut *data.writers.lock().unwrap());
    for writer in writers {
        writer.await.unwrap().unwrap();
    }
    drop(provider);
    assert!(lease_original.upgrade().is_none());
    drop(legacy);
    drop(watching);
    drop(fresh);
    drop(group);
    drop(session);
    client.close().await.unwrap();
    server.close().await.unwrap();
    bus.kill().await.unwrap();
    bus.wait().await.unwrap();
}
