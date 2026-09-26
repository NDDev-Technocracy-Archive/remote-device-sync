// Real relay/server binaries. Including targets supply BINARY, BIND_FLAG and
// HOSTS_DIRECTORY. Every process, socket and state directory belongs to a fixture.
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rds-relay-cli-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self(root)
    }
    fn key(&self) -> String {
        self.0.join("relay.key").to_str().unwrap().into()
    }
    fn spawn(&self, args: &[String]) -> Process {
        let stdout = self.0.join("stdout.log");
        let stderr = self.0.join("stderr.log");
        let log = |path: &Path| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .unwrap()
        };
        let mut command = Command::new(BINARY);
        command
            .args([BIND_FLAG, "127.0.0.1:0"])
            .env("RUST_LOG", "rds_relay=info,rds_server=info")
            .env("RDS_LOG_FORMAT", "text")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(log(&stdout))
            .stderr(log(&stderr));
        if HOSTS_DIRECTORY {
            command
                .args(["--http-addr", "127.0.0.1:0", "--directory"])
                .arg(self.0.join("records"));
        }
        command.args(args);
        Process {
            child: command.spawn().unwrap(),
            stdout,
            stderr,
        }
    }
    fn no_state(&self) {
        assert!(
            !self.0.join("relay.key").exists(),
            "invalid config created identity"
        );
        assert!(
            !self.0.join(".rds-key-transaction.lock").exists(),
            "invalid config entered identity transaction"
        );
        assert!(
            !self.0.join("records").exists(),
            "invalid config created catalog"
        );
        assert!(
            !self.0.join("records.policy").exists(),
            "invalid config created policy state"
        );
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn read_log(path: &Path) -> String {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() <= 64 * 1024,
        "fixture binary exceeded log budget"
    );
    // The formatter may include SGR sequences even if NO_COLOR is inherited.
    let mut clean = String::new();
    let decoded = String::from_utf8(bytes).unwrap();
    let mut chars = decoded.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            clean.push(c);
        }
    }
    clean
}
fn address(line: &str) -> SocketAddr {
    let start = line.find("127.0.0.1:").unwrap();
    line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ':'))
        .collect::<String>()
        .parse()
        .unwrap()
}
struct Ready {
    addr: SocketAddr,
    id: Option<iroh::EndpointId>,
    directory: Option<SocketAddr>,
}
impl Process {
    fn listener_output(&self) -> String {
        // The standalone relay prints its addresses as command output; the
        // composed server publishes private listener diagnostics on stderr.
        read_log(if HOSTS_DIRECTORY {
            &self.stderr
        } else {
            &self.stdout
        })
    }
    async fn exited(&mut self) -> ExitStatus {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned child did not exit")
    }
    async fn ready(&mut self, owned: bool) -> Ready {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "child startup failed: {}",
                    read_log(&self.stderr)
                );
                let output = self.listener_output();
                let mut addr = None;
                let mut id = None;
                let mut directory = None;
                for line in output.lines() {
                    if line.contains("discovery directory listening") {
                        directory = Some(address(line));
                    } else if line.contains("relay listening") {
                        addr = Some(address(line));
                    }
                    if let Some(key) = line.strip_prefix("relay endpoint id: ") {
                        id = key.trim().parse().ok();
                    }
                    if let Some((_, key)) = line.split_once("endpoint_id=") {
                        id = key.split_whitespace().next().unwrap().parse().ok();
                    }
                }
                if let Some(addr) = addr
                    && (!owned || id.is_some())
                {
                    assert_eq!(directory.is_some(), HOSTS_DIRECTORY);
                    return Ready {
                        addr,
                        id,
                        directory,
                    };
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned child never announced readiness")
    }
    async fn stop(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "child exited before requested shutdown"
        );
        let pid = rustix::process::Pid::from_raw(self.child.id().try_into().unwrap()).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        assert!(
            self.exited().await.success(),
            "child shutdown failed: {}",
            read_log(&self.stderr)
        );
    }
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}
async fn rejected(scratch: &Scratch, values: Vec<String>) -> String {
    let mut process = scratch.spawn(&values);
    assert!(
        !process.exited().await.success(),
        "invalid configuration was accepted"
    );
    scratch.no_state();
    read_log(&process.stderr)
}

#[tokio::test]
async fn invalid_or_misapplied_relay_flags_do_not_initialize_state() {
    for case in 0..13 {
        let scratch = Scratch::new();
        let key = scratch.key();
        let allow = iroh::SecretKey::from_bytes(&[151; 32]).public().to_string();
        let values = match case {
            0 => args(&["--relay-backend", "unknown"]),
            1 => args(&["--relay-key-file", &key]),
            2 => args(&["--relay-max-connections", "1"]),
            3 => args(&["--development-open-relay"]),
            4 => args(&["--tls-https-addr", "127.0.0.1:0"]),
            5 => args(&["--tls-acme-staging"]),
            6 => args(&["--relay-backend", "noq", "--relay-key-file", &key]),
            7 => args(&["--relay-backend", "noq", "--development-open-relay"]),
            8 => args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                &key,
                "--tls-https-addr",
                "127.0.0.1:0",
            ]),
            9 => args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                &key,
                "--relay-max-connections",
                "0",
            ]),
            10 => args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                &key,
                "--relay-max-connections",
                "65536",
            ]),
            11 => args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                &key,
                "--allow",
                &allow,
            ]),
            _ => args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                scratch.0.join(".rds-key-alias").to_str().unwrap(),
            ]),
        };
        assert!(!rejected(&scratch, values).await.is_empty());
    }
}

#[tokio::test]
async fn unavailable_backend_is_explicit_and_does_not_initialize_state() {
    let scratch = Scratch::new();
    // Probe the relay library, not this package's own feature flag:
    // workspace feature unification can compile the owned backend into
    // the spawned binary while `rds-server`'s `owned-relay` flag is off,
    // and then a valid owned flag set starts a live relay instead of
    // being rejected. Either way the contract is the same — an explicit
    // rejection that creates no state.
    let (values, needle) = if rds_relay::OWNED_BACKEND_COMPILED {
        (
            args(&["--relay-backend", "noq"]),
            "owned relay requires",
        )
    } else {
        (
            args(&[
                "--relay-backend",
                "noq",
                "--development-open-relay",
                "--relay-key-file",
                &scratch.key(),
            ]),
            "backend unavailable",
        )
    };
    let error = rejected(&scratch, values).await;
    assert!(error.contains(needle), "{error}");
}

#[tokio::test]
async fn malformed_or_oversized_relay_pem_is_rejected_before_catalog_creation() {
    for oversized in [false, true] {
        let scratch = Scratch::new();
        let cert = scratch.0.join("cert.pem");
        let key = scratch.0.join("tls.key");
        fs::write(
            &cert,
            if oversized {
                vec![b'x'; 1024 * 1024 + 1]
            } else {
                b"invalid PEM".to_vec()
            },
        )
        .unwrap();
        fs::write(&key, b"invalid key").unwrap();
        let error = rejected(
            &scratch,
            args(&[
                "--tls-cert",
                cert.to_str().unwrap(),
                "--tls-key",
                key.to_str().unwrap(),
                "--tls-https-addr",
                "127.0.0.1:0",
            ]),
        )
        .await;
        assert!(error.contains("relay TLS configuration"), "{error}");
    }
}

#[tokio::test]
async fn explicit_iroh_mode_preserves_listener_and_shutdown_contract() {
    let scratch = Scratch::new();
    let mut process = scratch.spawn(&args(&["--relay-backend", "iroh"]));
    let ready = process.ready(false).await;
    assert!(ready.id.is_none());
    if let Some(addr) = ready.directory {
        rds_discovery::client::Client::new(addr)
            .health()
            .await
            .unwrap();
    }
    process.stop().await;
    let _rebound = std::net::TcpListener::bind(ready.addr).unwrap();
}

fn tls_fixture(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = scratch.0.join("cert.pem");
    let key_path = scratch.0.join("tls.key");
    fs::write(&cert_path, cert.pem()).unwrap();
    fs::write(&key_path, signing_key.serialize_pem()).unwrap();
    (cert_path, key_path)
}

#[tokio::test]
async fn mismatched_or_oversized_private_key_is_rejected_before_catalog_creation() {
    for oversized in [false, true] {
        let scratch = Scratch::new();
        let (cert, key) = tls_fixture(&scratch);
        let replacement = if oversized {
            vec![b'x'; 64 * 1024 + 1]
        } else {
            rcgen::KeyPair::generate()
                .unwrap()
                .serialize_pem()
                .into_bytes()
        };
        fs::write(&key, replacement).unwrap();
        let error = rejected(
            &scratch,
            args(&[
                "--tls-cert",
                cert.to_str().unwrap(),
                "--tls-key",
                key.to_str().unwrap(),
                "--tls-https-addr",
                "127.0.0.1:0",
            ]),
        )
        .await;
        assert!(error.contains("relay TLS configuration"), "{error}");
    }
}

#[tokio::test]
async fn valid_manual_iroh_tls_starts_and_shuts_down() {
    let scratch = Scratch::new();
    let (cert, key) = tls_fixture(&scratch);
    let mut process = scratch.spawn(&args(&[
        "--tls-cert",
        cert.to_str().unwrap(),
        "--tls-key",
        key.to_str().unwrap(),
        "--tls-https-addr",
        "127.0.0.1:0",
    ]));
    process.ready(false).await;
    let addr = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let output = process.listener_output();
            if let Some(line) = output.lines().find(|line| {
                line.contains("relay tls listening") || line.contains("relay tls url:")
            }) {
                break address(line);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("TLS listener not announced");
    let _connected = tokio::net::TcpStream::connect(addr).await.unwrap();
    process.stop().await;
    let _rebound = std::net::TcpListener::bind(addr).unwrap();
}

#[cfg(feature = "owned-relay")]
fn owned_args(scratch: &Scratch, keys: &[iroh::SecretKey], cap: u16) -> Vec<String> {
    let mut values = args(&[
        "--relay-backend",
        "noq",
        "--relay-key-file",
        &scratch.key(),
        "--relay-max-connections",
        &cap.to_string(),
    ]);
    for key in keys {
        values.extend(args(&["--allow", &key.public().to_string()]));
    }
    values
}

#[cfg(feature = "owned-relay")]
fn relay_address(ready: &Ready) -> iroh::EndpointAddr {
    iroh::EndpointAddr::new(ready.id.unwrap()).with_ip_addr(ready.addr)
}

#[cfg(feature = "owned-relay")]
async fn attached_endpoint(key: iroh::SecretKey, relay: iroh::EndpointAddr) -> rds_net::Endpoint {
    rds_net::bind_endpoint(rds_net::EndpointConfig {
        backend: rds_net::Backend::Noq,
        secret_key: Some(key),
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        discovery: false,
        relay_endpoint: Some(relay),
        // The relay-only handshake occupies the only path, so a direct child
        // cannot subsequently bypass this fixture's relay during transfer.
        max_multipath_paths: Some(1),
        ..Default::default()
    })
    .await
    .unwrap()
}

#[cfg(feature = "owned-relay")]
async fn attachment(
    relay: iroh::EndpointAddr,
    key: iroh::SecretKey,
) -> anyhow::Result<(
    rds_net::backends::noq::relay::RelaySocket,
    rds_net::backends::noq::relay::RelayHandle,
)> {
    rds_net::backends::noq::relay::RelaySocket::connect(relay, key, "127.0.0.1:0".parse().unwrap())
        .await
}

#[cfg(feature = "owned-relay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_binary_forwards_encrypted_traffic_and_reuses_identity_after_restart() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let scratch = Scratch::new();
        let ka = iroh::SecretKey::from_bytes(&[151; 32]);
        let kb = iroh::SecretKey::from_bytes(&[152; 32]);
        let mut values = owned_args(&scratch, &[ka.clone(), kb.clone()], 2);
        if HOSTS_DIRECTORY {
            values.extend(args(&[
                "--directory-allow",
                &rds_discovery::EndpointKey(*kb.public().as_bytes()).to_string(),
            ]));
        }
        // One issuer survives both fixture server runs. Restarting it at zero
        // would conceal durable directory revision semantics.
        let mut issuer = rds_discovery::RecordIssuer::memory(
            ed25519_dalek::SigningKey::from_bytes(&kb.to_bytes()),
        );
        let mut identity = None;
        for _ in 0..2 {
            let mut process = scratch.spawn(&values);
            let ready = process.ready(true).await;
            if let Some(previous) = identity {
                assert_eq!(ready.id, Some(previous));
            }
            identity = ready.id;
            let relay = relay_address(&ready);
            // Deny unknown identities before filling capacity: this observes
            // the allowlist, not an unrelated overload rejection.
            let denied = tokio::time::timeout(
                Duration::from_secs(5),
                attachment(relay.clone(), iroh::SecretKey::from_bytes(&[153; 32])),
            )
            .await;
            assert!(
                matches!(denied, Ok(Err(_))),
                "unknown peer was not promptly rejected"
            );
            let a = attached_endpoint(ka.clone(), relay.clone()).await;
            let b = attached_endpoint(kb.clone(), relay).await;
            let mut target = b.addr();
            target
                .addrs
                .retain(|address| matches!(address, iroh::TransportAddr::Relay(_)));
            assert_eq!(target.addrs.len(), 1);
            if let Some(addr) = ready.directory {
                let client = rds_discovery::client::Client::new(addr);
                let record = issuer
                    .record(
                        rds_discovery::RecordDraft {
                            addrs: vec![],
                            relay_urls: target
                                .addrs
                                .iter()
                                .filter_map(|addr| match addr {
                                    iroh::TransportAddr::Relay(url) => Some(url.to_string()),
                                    _ => None,
                                })
                                .collect(),
                            services: vec![rds_discovery::Service::Ping],
                            ttl: Duration::from_secs(120),
                        },
                        rds_discovery::now_unix().unwrap(),
                    )
                    .unwrap();
                client.publish(&record).await.unwrap();
                let resolved = rds_net::resolve_target(Some(client), &b.id().to_string())
                    .await
                    .unwrap();
                assert_eq!(resolved, target);
                target = resolved;
            }
            let traffic = tokio::time::timeout(Duration::from_secs(8), async {
                let (local, remote) = tokio::try_join!(a.connect(target, b"rds/0"), async {
                    b.accept()
                        .await
                        .ok_or_else(|| anyhow::anyhow!("fixture endpoint closed"))?
                        .await
                })?;
                assert_eq!(local.remote_id(), b.id());
                assert_eq!(remote.remote_id(), a.id());
                let (mut send, mut recv) = local.open_bi().await?;
                send.write_all(b"request").await?;
                send.finish()?;
                let (mut reply, mut request) = remote.accept_bi().await?;
                assert_eq!(request.read_to_end(7).await?, b"request");
                reply.write_all(b"response").await?;
                reply.finish()?;
                assert_eq!(recv.read_to_end(8).await?, b"response");
                local.send_datagram(b"datagram".to_vec().into())?;
                assert_eq!(&remote.read_datagram().await?[..], b"datagram");
                let paths = [local.path_stats(), remote.path_stats()];
                let registry = a.metrics();
                let mut sampler = registry.sampler(local.clone());
                sampler.sample();
                let counters = registry.snapshot();
                local.close(0u32.into(), b"fixture complete");
                remote.close(0u32.into(), b"fixture complete");
                Ok::<_, anyhow::Error>((paths, counters))
            })
            .await;
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(a.close(), b.close());
            })
            .await
            .unwrap();
            process.stop().await;
            assert!(
                matches!(&traffic, Ok(Ok(_))),
                "relay traffic failed: {traffic:?}"
            );
            let (paths, counters) = traffic.unwrap().unwrap();
            assert_eq!(counters["rds_net_bytes_sent_total{via=\"direct\"}"], 0);
            assert_eq!(counters["rds_net_bytes_received_total{via=\"direct\"}"], 0);
            assert!(counters["rds_net_bytes_sent_total{via=\"relay\"}"] > 0);
            assert!(counters["rds_net_bytes_received_total{via=\"relay\"}"] > 0);
            assert_eq!(counters["rds_net_policy_observed_connections"], 1);
            assert_eq!(counters["rds_net_degraded_path_observers"], 0);
            assert_eq!(counters["rds_net_selected_path_known"], 1);
            for paths in paths {
                assert_eq!(paths.len(), 1, "single-path fixture");
                assert!(paths[0].via_relay, "relay traffic was labeled direct");
                assert!(paths[0].selected);
                assert!(paths[0].sent_bytes > 0 && paths[0].recv_bytes > 0);
            }
            let _rebound = std::net::UdpSocket::bind(ready.addr).unwrap();
        }
    })
    .await
    .expect("owned binary restart fixture stalled");
}

#[cfg(feature = "owned-relay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_binary_capacity_recovers_after_disconnect() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let scratch = Scratch::new();
        let ka = iroh::SecretKey::from_bytes(&[154; 32]);
        let kb = iroh::SecretKey::from_bytes(&[155; 32]);
        let mut process = scratch.spawn(&owned_args(&scratch, &[ka.clone(), kb.clone()], 1));
        let relay = relay_address(&process.ready(true).await);
        let (first, _) = attachment(relay.clone(), ka).await.unwrap();
        assert!(
            attachment(relay.clone(), kb.clone()).await.is_err(),
            "capacity flag was ignored"
        );
        drop(first);
        let (second, _) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(socket) = attachment(relay.clone(), kb.clone()).await {
                    break socket;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("connection capacity was not returned");
        drop(second);
        process.stop().await;
    })
    .await
    .expect("owned capacity fixture stalled");
}

#[cfg(feature = "owned-relay")]
#[tokio::test]
async fn explicit_development_open_mode_accepts_unlisted_peer_and_delivers_drain() {
    let scratch = Scratch::new();
    let mut process = scratch.spawn(&args(&[
        "--relay-backend",
        "noq",
        "--relay-key-file",
        &scratch.key(),
        "--development-open-relay",
    ]));
    let relay = relay_address(&process.ready(true).await);
    let (socket, handle) = attachment(relay, iroh::SecretKey::from_bytes(&[156; 32]))
        .await
        .unwrap();
    process.stop().await;
    // Child exit does not synchronize the client's QUIC receive/control tasks.
    // Drain deliberately keeps the tunnel usable during grace. Require actual
    // local closure under a bounded deadline, without treating Drain as close.
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.is_available() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("client did not observe relay closure after process exit");
    assert!(
        handle.drained(),
        "binary did not deliver checked drain to its client"
    );
    assert!(!handle.is_available());
    drop(socket);
}
