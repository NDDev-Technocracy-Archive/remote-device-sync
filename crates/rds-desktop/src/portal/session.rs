//! One consented combined session, with a retained bounded close owner.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use futures_util::StreamExt;
use tokio::sync::{Semaphore, oneshot, watch};

use crate::DesktopError;

const APP_ID: &str = "com.nddev.RDS.Agent";
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
// Local consent is user-paced; two physical displays may be on another desk.
// It stays bounded, and remote peers cannot initiate this preparation.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(3600);
pub(super) const MAX_MONITORS: usize = 16;

/// The persistent ID belongs to restored sessions; the mapping ID belongs to
/// the current EIS region. Neither is a PipeWire node identity across sessions.
pub(super) struct Monitor {
    pub stable_id: String,
    pub mapping_id: String,
    pub node: u32,
}

pub(super) struct Granted {
    pub owner: Owner,
    pub monitors: Vec<Monitor>,
    pub video: OwnedFd,
    pub input: OwnedFd,
    pub restore_token: String,
    pub clipboard: Option<Arc<super::clipboard::Group>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Closed {
    Requested,
    Revoked,
    Disconnected,
}

pub(super) struct Owner {
    pending: Arc<Mutex<Option<ashpd::zbus::zvariant::OwnedObjectPath>>>,
    close: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    pub closed: watch::Receiver<Option<Closed>>,
}

impl Owner {
    pub async fn close(&mut self) {
        self.request_close();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    pub fn request_close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Cancellation cannot abort the close owner. It keeps the session and
        // its admission permit until bounded close/connection shutdown ends.
        self.request_close();
    }
}

fn error(stage: &str) -> DesktopError {
    // Portal responses may contain permission tokens. Do not format them.
    DesktopError::Capture(format!("Wayland portal {stage} failed"))
}

async fn rpc<T>(
    stage: &str,
    future: impl Future<Output = ashpd::Result<T>>,
) -> Result<T, DesktopError> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .map_err(|_| error(stage))?
        .map_err(|_| error(stage))
}

fn cursor_mode(value: Option<&std::ffi::OsStr>) -> Result<CursorMode, DesktopError> {
    match value {
        None => Ok(CursorMode::Embedded),
        Some(value) if value == "embedded" => Ok(CursorMode::Embedded),
        Some(value) if value == "hidden" => Ok(CursorMode::Hidden),
        Some(_) => Err(DesktopError::Capture(
            "invalid RDS_PORTAL_CURSOR_MODE; expected embedded or hidden".into(),
        )),
    }
}
fn require_cursor_mode(
    available: ashpd::enumflags2::BitFlags<CursorMode>,
    selected: CursorMode,
) -> Result<(), DesktopError> {
    if available.contains(selected) {
        Ok(())
    } else {
        Err(error("requested cursor mode unsupported"))
    }
}

pub(super) async fn open(
    restore_token: Option<&str>,
    cancelled: impl Future<Output = ()>,
) -> Result<Granted, DesktopError> {
    let cursor_mode = cursor_mode(std::env::var_os("RDS_PORTAL_CURSOR_MODE").as_deref())?;
    tracing::info!(
        mode = match cursor_mode {
            CursorMode::Hidden => "hidden",
            _ => "embedded",
        },
        "Wayland portal capture cursor mode selected"
    );
    tokio::pin!(cancelled);
    static SESSIONS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let admission = tokio::time::timeout(
        RPC_TIMEOUT,
        SESSIONS
            .get_or_init(|| Arc::new(Semaphore::new(4)))
            .clone()
            .acquire_owned(),
    );
    let permit = tokio::select! {
        biased;
        _ = &mut cancelled => return Err(error("preparation cancelled")),
        permit = admission => permit.map_err(|_| error("admission"))?.map_err(|_| error("admission"))?,
    };
    // This connection is exclusively ours. Cancellation before CreateSession
    // returns drops it, rather than leaving a process-global portal peer alive.
    let connection = tokio::select! {
        _ = &mut cancelled => return Err(error("preparation cancelled")),
        connection = tokio::time::timeout(RPC_TIMEOUT, ashpd::zbus::Connection::session()) => {
            connection.map_err(|_| error("bus connection"))?.map_err(|_| error("bus connection"))?
        },
    };
    let devices = DeviceType::Keyboard | DeviceType::Pointer;
    let creating = async {
        rpc(
            "application registration",
            ashpd::register_host_app_with_connection(
                connection.clone(),
                APP_ID.parse().map_err(|_| error("application identity"))?,
            ),
        )
        .await?;
        let remote = rpc(
            "remote-desktop proxy",
            RemoteDesktop::with_connection(connection.clone()),
        )
        .await?;
        let screencast = rpc(
            "screencast proxy",
            Screencast::with_connection(connection.clone()),
        )
        .await?;
        if remote.version() < 2 || screencast.version() < 5 {
            return Err(error("required EIS/mapping protocol"));
        }
        let available = rpc("input capabilities", remote.available_device_types()).await?;
        let cursors = rpc("cursor capabilities", screencast.available_cursor_modes()).await?;
        if !available.contains(devices) {
            return Err(error("required input capabilities"));
        }
        require_cursor_mode(cursors, cursor_mode)?;
        let session = Arc::new(
            rpc(
                "session creation",
                remote.create_session(Default::default()),
            )
            .await?,
        );
        Ok::<_, DesktopError>((remote, screencast, session))
    };
    let created = tokio::select! {
        result = creating => result,
        _ = &mut cancelled => Err(error("preparation cancelled")),
    };
    let (remote, screencast, session) = match created {
        Ok(parts) => parts,
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_secs(2), connection.close()).await;
            return Err(error);
        }
    };
    let connection_for_start = connection.clone();
    let mut owner = own_session(session.clone(), connection, permit);
    let configured = async {
        rpc(
            "input selection",
            remote.select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(devices)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(restore_token),
            ),
        )
        .await?
        .response()
        .map_err(|_| error("input selection"))?;
        // Combined-session persistence belongs exclusively to RemoteDesktop.
        rpc(
            "monitor selection",
            screencast.select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_sources(Some(SourceType::Monitor.into()))
                    .set_multiple(true)
                    .set_cursor_mode(cursor_mode),
            ),
        )
        .await?
        .response()
        .map_err(|_| error("monitor selection"))?;
        let clipboard = match rpc(
            "clipboard proxy",
            ashpd::desktop::clipboard::Clipboard::with_connection(connection_for_start.clone()),
        )
        .await
        {
            Ok(proxy) if proxy.version() >= 1 => {
                if rpc(
                    "clipboard request",
                    proxy.request(&session, Default::default()),
                )
                .await
                .is_ok()
                {
                    Some(proxy)
                } else {
                    None
                }
            }
            _ => None,
        };
        let selected = start(&remote, &session, &owner, &connection_for_start).await?;
        if !selected.devices().contains(devices)
            || selected.streams().is_empty()
            || selected.streams().len() > MAX_MONITORS
            || owner.closed.borrow().is_some()
        {
            return Err(error("granted capabilities"));
        }
        let mut monitors = Vec::with_capacity(selected.streams().len());
        for stream in selected.streams() {
            let stable = stream
                .id()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or_else(|| error("stable display identity"))?;
            let mapping = stream
                .mapping_id()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or_else(|| error("input mapping identity"))?;
            if stream.source_type() != Some(SourceType::Monitor)
                || matches!(stream.pipe_wire_node_id(), 0 | u32::MAX)
                || monitors.iter().any(|m: &Monitor| {
                    m.stable_id == stable
                        || m.mapping_id == mapping
                        || m.node == stream.pipe_wire_node_id()
                })
            {
                return Err(error("ambiguous display identity"));
            }
            monitors.push(Monitor {
                stable_id: stable.into(),
                mapping_id: mapping.into(),
                node: stream.pipe_wire_node_id(),
            });
        }
        let restore_token = selected
            .restore_token()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or_else(|| error("persistent permission"))?
            .to_owned();
        let video = rpc(
            "video connection",
            screencast.open_pipe_wire_remote(&session, Default::default()),
        )
        .await?;
        let input = rpc(
            "input connection",
            remote.connect_to_eis(&session, Default::default()),
        )
        .await?;
        let clipboard = if selected.is_clipboard_enabled() {
            match clipboard {
                Some(proxy) => match super::clipboard::Group::start(proxy, session.clone()).await {
                    Ok(group) => Some(Arc::new(group)),
                    Err(_) => {
                        tracing::warn!("Wayland clipboard signal subscription unavailable");
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };
        Ok::<_, DesktopError>((monitors, video, input, restore_token, clipboard))
    };
    let configured = tokio::select! {
        result = configured => result,
        _ = &mut cancelled => Err(error("preparation cancelled")),
    };
    let (monitors, video, input, restore_token, clipboard) = match configured {
        Ok(parts) => parts,
        Err(error) => {
            owner.close().await;
            return Err(error);
        }
    };
    Ok(Granted {
        owner,
        monitors,
        video,
        input,
        restore_token,
        clipboard,
    })
}

fn own_session(
    closing_session: Arc<Session<RemoteDesktop>>,
    connection: ashpd::zbus::Connection,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Owner {
    let (close, stop) = oneshot::channel();
    let (closed_tx, closed) = watch::channel(None);
    let pending = Arc::new(Mutex::new(None));
    let closing_pending = pending.clone();
    let task = tokio::spawn(async move {
        let _permit = permit;
        let signals = tokio::time::timeout(RPC_TIMEOUT, closing_session.receive_closed()).await;
        let reason = if let Ok(Ok(signals)) = signals {
            tokio::pin!(signals);
            tokio::select! {
                _ = stop => Closed::Requested,
                signal = signals.next() => if signal.is_some() { Closed::Revoked } else { Closed::Disconnected },
            }
        } else {
            Closed::Disconnected
        };
        tracing::info!(?reason, "Wayland portal session closed");
        closed_tx.send_replace(Some(reason));
        let pending = closing_pending
            .lock()
            .ok()
            .and_then(|mut value| value.take());
        if let Some(path) = pending {
            close_request(&connection, path).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), closing_session.close()).await;
        // Explicitly close the unique peer even if the portal didn't answer Close.
        let _ = tokio::time::timeout(Duration::from_secs(2), connection.close()).await;
    });
    Owner {
        pending,
        close: Some(close),
        task: Some(task),
        closed,
    }
}

// ashpd's convenience Start awaits user Response before returning its Request,
// so cancellation otherwise loses the handle needed to close the dialog. Keep
// the standard request path in our existing close owner before sending Start.
async fn start(
    _remote: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    owner: &Owner,
    connection: &ashpd::zbus::Connection,
) -> Result<ashpd::desktop::remote_desktop::SelectedDevices, DesktopError> {
    use ashpd::desktop::Response;
    use ashpd::zbus::zvariant::{OwnedObjectPath, Value};
    let sender = connection
        .unique_name()
        .ok_or_else(|| error("request peer"))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let token = format!("rds_{:032x}", rand::random::<u128>());
    let path = OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
    .map_err(|_| error("request identity"))?;
    *owner.pending.lock().map_err(|_| error("request owner"))? = Some(path.clone());
    let request = tokio::time::timeout(
        RPC_TIMEOUT,
        ashpd::zbus::Proxy::new(
            connection,
            "org.freedesktop.portal.Desktop",
            path.clone(),
            "org.freedesktop.portal.Request",
        ),
    )
    .await
    .map_err(|_| error("response proxy deadline"))?
    .map_err(|_| error("response proxy"))?;
    let mut responses = tokio::time::timeout(RPC_TIMEOUT, request.receive_signal("Response"))
        .await
        .map_err(|_| error("response subscription deadline"))?
        .map_err(|_| error("response subscription"))?;
    let remote = tokio::time::timeout(
        RPC_TIMEOUT,
        ashpd::zbus::Proxy::new(
            connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.RemoteDesktop",
        ),
    )
    .await
    .map_err(|_| error("start proxy deadline"))?
    .map_err(|_| error("start proxy"))?;
    let options = std::collections::HashMap::from([("handle_token", Value::from(token))]);
    let returned: OwnedObjectPath =
        tokio::time::timeout(RPC_TIMEOUT, remote.call("Start", &(session, "", options)))
            .await
            .map_err(|_| error("start RPC deadline"))?
            .map_err(|_| error("start RPC"))?;
    if returned != path {
        return Err(error("request identity mismatch"));
    }
    tracing::info!("Wayland local consent request is waiting for its Response");
    let mut closed = owner.closed.clone();
    if closed.borrow().is_some() {
        return Err(error("closed before consent"));
    }
    let message = tokio::select! {
        message = tokio::time::timeout(CONSENT_TIMEOUT, responses.next()) => message.map_err(|_| error("consent deadline"))?.ok_or_else(|| error("response stream ended"))?,
        _ = closed.changed() => return Err(error("closed during consent")),
    };
    owner
        .pending
        .lock()
        .map_err(|_| error("request owner"))?
        .take();
    tracing::info!("Wayland local consent Response signal arrived");
    let response: Response<ashpd::desktop::remote_desktop::SelectedDevices> = message
        .body()
        .deserialize()
        .map_err(|_| error("consent response decode"))?;
    tracing::info!("Wayland local consent Response received");
    match response {
        Response::Ok(selected) => Ok(selected),
        Response::Err(_) => Err(error("consent response")),
    }
}

async fn close_request(
    connection: &ashpd::zbus::Connection,
    path: ashpd::zbus::zvariant::OwnedObjectPath,
) {
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let proxy = ashpd::zbus::Proxy::new(
            connection,
            "org.freedesktop.portal.Desktop",
            path,
            "org.freedesktop.portal.Request",
        )
        .await?;
        proxy.call::<_, _, ()>("Close", &()).await
    })
    .await;
}

#[cfg(test)]
mod request_tests {
    use super::*;
    #[test]
    fn explicit_cursor_modes_are_validated_without_fallback_or_global_environment_changes() {
        use std::ffi::OsStr;
        let embedded = cursor_mode(None).unwrap();
        assert_eq!(embedded, CursorMode::Embedded);
        let hidden = cursor_mode(Some(OsStr::new("hidden"))).unwrap();
        assert_eq!(hidden, CursorMode::Hidden);
        assert!(require_cursor_mode(CursorMode::Embedded.into(), hidden).is_err());
        assert!(require_cursor_mode(CursorMode::Embedded | CursorMode::Hidden, hidden).is_ok());
        let request = SelectSourcesOptions::default().set_cursor_mode(hidden);
        assert_eq!(
            serde_json::to_value(request).unwrap()["cursor_mode"]["value"],
            1
        );
        assert!(cursor_mode(Some(OsStr::new("metadata"))).is_err());
        assert!(cursor_mode(Some(OsStr::new(""))).is_err());
        assert!(cursor_mode(Some(OsStr::new("Hidden"))).is_err());
    }

    use ashpd::zbus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    struct EarlyResponsePortal {
        response: u32,
        wrong_path: bool,
    }

    fn fixture_path(
        header: &zbus::message::Header<'_>,
        options: &std::collections::HashMap<String, OwnedValue>,
        segment: &str,
        token_key: &str,
    ) -> OwnedObjectPath {
        let sender = header
            .sender()
            .unwrap()
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = <&str>::try_from(options.get(token_key).unwrap()).unwrap();
        OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/{segment}/{sender}/{token}"
        ))
        .unwrap()
    }

    #[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop", crate = "ashpd::zbus")]
    impl EarlyResponsePortal {
        #[zbus(property)]
        fn version(&self) -> u32 {
            2
        }

        async fn create_session(
            &self,
            options: std::collections::HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let request = fixture_path(&header, &options, "request", "handle_token");
            let session = fixture_path(&header, &options, "session", "session_handle_token");
            connection
                .emit_signal(
                    header.sender().cloned(),
                    request.clone(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(
                        0_u32,
                        std::collections::HashMap::from([(
                            "session_handle",
                            OwnedValue::from(session.into_inner()),
                        )]),
                    ),
                )
                .await?;
            Ok(request)
        }

        async fn start(
            &self,
            _session: OwnedObjectPath,
            _parent: &str,
            options: std::collections::HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let path = fixture_path(&header, &options, "request", "handle_token");
            // Deliberately precede the method reply. There is no user pacing,
            // helper task or sleep which could hide a subscription race.
            connection
                .emit_signal(
                    header.sender().cloned(),
                    path.clone(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(
                        self.response,
                        std::collections::HashMap::from([("devices", OwnedValue::from(3_u32))]),
                    ),
                )
                .await?;
            Ok(if self.wrong_path {
                OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/wrong").unwrap()
            } else {
                path
            })
        }
    }

    async fn check_early_response(response: u32, wrong_path: bool) {
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
        let server = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                EarlyResponsePortal {
                    response,
                    wrong_path,
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let remote = RemoteDesktop::with_connection(client.clone())
            .await
            .unwrap();
        let session = tokio::time::timeout(
            Duration::from_secs(3),
            remote.create_session(Default::default()),
        )
        .await
        .unwrap()
        .unwrap();
        let (_closed, closed) = watch::channel(None);
        let owner = Owner {
            pending: Arc::new(Mutex::new(None)),
            close: None,
            task: None,
            closed,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            start(&remote, &session, &owner, &client),
        )
        .await
        .expect("an early directed Response must not be lost");
        if wrong_path {
            assert!(
                matches!(result, Err(DesktopError::Capture(ref message)) if message.contains("request identity mismatch"))
            );
            assert!(owner.pending.lock().unwrap().is_some());
        } else {
            assert!(owner.pending.lock().unwrap().is_none());
            if response == 0 {
                assert!(
                    result
                        .unwrap()
                        .devices()
                        .contains(DeviceType::Keyboard | DeviceType::Pointer)
                );
            } else {
                assert!(
                    matches!(result, Err(DesktopError::Capture(ref message)) if message.contains("consent response"))
                );
            }
        }
        client.close().await.unwrap();
        server.close().await.unwrap();
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }

    #[tokio::test]
    async fn directed_consent_response_before_start_reply_is_received() {
        check_early_response(0, false).await;
    }

    #[tokio::test]
    async fn early_consent_denial_does_not_wait_or_reopen() {
        check_early_response(1, false).await;
    }

    #[tokio::test]
    async fn returned_request_identity_cannot_replace_the_owned_path() {
        check_early_response(0, true).await;
    }

    struct Request(Arc<AtomicUsize>);
    #[zbus::interface(name = "org.freedesktop.portal.Request", crate = "ashpd::zbus")]
    impl Request {
        async fn close(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct OrderedRequest(Arc<Mutex<Vec<&'static str>>>);
    #[zbus::interface(name = "org.freedesktop.portal.Request", crate = "ashpd::zbus")]
    impl OrderedRequest {
        fn close(&self) {
            self.0.lock().unwrap().push("request");
        }
    }
    struct OrderedSession(Arc<Mutex<Vec<&'static str>>>);
    #[zbus::interface(name = "org.freedesktop.portal.Session", crate = "ashpd::zbus")]
    impl OrderedSession {
        fn close(&self) {
            self.0.lock().unwrap().push("session");
        }
    }

    #[tokio::test]
    async fn local_stop_joins_request_session_connection_and_admission_cleanup() {
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
        let server = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                EarlyResponsePortal {
                    response: 0,
                    wrong_path: false,
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let remote = RemoteDesktop::with_connection(client.clone())
            .await
            .unwrap();
        let session = Arc::new(remote.create_session(Default::default()).await.unwrap());
        let events = Arc::new(Mutex::new(Vec::new()));
        // CreateSession returned this exact SDK-owned path on our private bus.
        let session_path = serde_json::to_value(&*session).unwrap();
        let session_path =
            OwnedObjectPath::try_from(session_path.as_str().unwrap().to_owned()).unwrap();
        server
            .object_server()
            .at(session_path, OrderedSession(events.clone()))
            .await
            .unwrap();
        let sender = client
            .unique_name()
            .unwrap()
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let request_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/pending"
        ))
        .unwrap();
        server
            .object_server()
            .at(request_path.clone(), OrderedRequest(events.clone()))
            .await
            .unwrap();
        let admission = Arc::new(Semaphore::new(1));
        let permit = admission.clone().acquire_owned().await.unwrap();
        let mut owner = own_session(session, client.clone(), permit);
        *owner.pending.lock().unwrap() = Some(request_path);
        assert_eq!(admission.available_permits(), 0);
        tokio::time::timeout(Duration::from_secs(3), owner.close())
            .await
            .unwrap();
        assert_eq!(*events.lock().unwrap(), ["request", "session"]);
        assert!(client.is_closed());
        assert_eq!(*owner.closed.borrow(), Some(Closed::Requested));
        assert_eq!(admission.available_permits(), 1);
        server.close().await.unwrap();
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }

    #[tokio::test]
    async fn pending_request_is_closed_on_its_owned_bus_without_a_user_response() {
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
        let calls = Arc::new(AtomicUsize::new(0));
        let path =
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_0/rds_test")
                .unwrap();
        let server = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(path.clone(), Request(calls.clone()))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        close_request(&client, path).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        client.close().await.unwrap();
        server.close().await.unwrap();
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }
}
