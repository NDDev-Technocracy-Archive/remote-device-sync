//! Native multi-session orchestration through the existing same-UID agent API.
use super::{Options, Resolution, native, read_grant};
use anyhow::Context;
use rds_client::local::Client;
use rds_core::local::{Command, Reply};
use rds_desktop::render::workspace::{DeviceView, TabId, TabSpec, VideoSize, WorkspaceModel};
use rds_desktop::render::{Viewer, WorkspaceEvent, WorkspaceHandle, WorkspaceUpdate};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

mod store;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub key: String,
    pub target: String,
    pub label: String,
    pub grant_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_addresses: Vec<std::net::SocketAddr>,
}

impl DeviceConfig {
    fn targets_peer(&self, peer: &str) -> bool {
        // A ticket and its manager session's bare identity name the same
        // computer. Keep the saved routes, profile key and grant reference.
        rds_net::parse_target(&self.target).is_ok_and(|target| target.id.to_string() == peer)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub presentation: Presentation,
    pub devices: Vec<DeviceConfig>,
    pub tabs: Vec<TabSpec>,
}
#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Presentation {
    #[default]
    Tabs,
    Spaces,
}
impl WorkspaceConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.schema_version == 1, "unsupported workspace version");
        anyhow::ensure!(
            self.devices.len() <= 32,
            "workspace has more than 32 devices"
        );
        let mut keys = std::collections::HashSet::new();
        for device in &self.devices {
            rds_core::local::validate_local_addresses(&device.local_addresses)?;
            anyhow::ensure!(
                device.local_addresses.is_empty() || rds_net::parse_target(&device.target).is_ok(),
                "local address preference requires a pinned peer"
            );
            anyhow::ensure!(keys.insert(&device.key), "duplicate workspace device");
            for value in [&device.key, &device.target] {
                anyhow::ensure!(
                    !value.is_empty()
                        && value.len() <= 8192
                        && !value.chars().any(char::is_control),
                    "invalid workspace device"
                );
            }
            anyhow::ensure!(
                !device.label.is_empty()
                    && device.label.len() <= 256
                    && !device.label.chars().any(char::is_control),
                "invalid device name"
            );
        }
        let mut model = WorkspaceModel::default();
        for tab in &self.tabs {
            anyhow::ensure!(
                keys.contains(&tab.device),
                "workspace tab has no device profile"
            );
            let (_, fresh) = model.open(tab.clone())?;
            anyhow::ensure!(fresh, "duplicate workspace tab");
        }
        Ok(())
    }

    pub async fn load(path: PathBuf) -> anyhow::Result<Option<Self>> {
        tokio::task::spawn_blocking(move || store::load(&path)).await?
    }
}

struct SessionTask {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl SessionTask {
    async fn finish(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(3), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

pub async fn run(
    client: Client,
    mut config: WorkspaceConfig,
    path: PathBuf,
    options: Options,
    all_displays: bool,
) -> anyhow::Result<()> {
    config.validate()?;
    if all_displays {
        if config.devices.is_empty() {
            let selected = client.selected(None).await?;
            let snapshot = client.snapshot().await?;
            let peer = snapshot
                .sessions
                .into_iter()
                .find(|s| s.id == selected)
                .context("selected computer unavailable")?
                .peer;
            config.devices.push(DeviceConfig {
                key: peer.clone(),
                target: peer,
                label: "Selected computer".into(),
                grant_file: None,
                local_addresses: vec![],
            });
        }
        let mut tabs = vec![];
        for device in &config.devices {
            let (_, inventory) = tokio::time::timeout(
                Duration::from_secs(20),
                inspect(
                    &client,
                    device.target.clone(),
                    device.grant_file.clone(),
                    device.local_addresses.clone(),
                ),
            )
            .await
            .context("display lookup timed out")??;
            let profile = config
                .tabs
                .iter()
                .find(|t| t.device == device.key)
                .map(|t| t.profile.clone())
                .unwrap_or_default();
            for display in inventory.displays {
                anyhow::ensure!(
                    tabs.len() < rds_desktop::render::workspace::MAX_TABS,
                    "at most eight displays can be open at once"
                );
                tabs.push(TabSpec {
                    device: device.key.clone(),
                    label: device.label.clone(),
                    display: display.index,
                    profile: profile.clone(),
                });
            }
        }
        config.tabs = tabs;
    }
    let devices = config
        .devices
        .iter()
        .map(|device| DeviceView {
            key: device.key.clone(),
            label: device.label.clone(),
            displays: vec![],
        })
        .collect();
    let (viewer, handle, events) = match config.presentation {
        Presentation::Tabs => Viewer::workspace(config.tabs.clone(), devices)?,
        Presentation::Spaces => Viewer::spaces(config.tabs.clone(), devices)?,
    };
    let stop = CancellationToken::new();
    let actor_stop = stop.clone();
    let task = tokio::spawn(async move {
        actor(client, config, path, options, handle, events, actor_stop).await
    });
    let result = tokio::task::block_in_place(|| viewer.run());
    stop.cancel();
    let mut task = task;
    if tokio::time::timeout(Duration::from_secs(5), &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
    result?;
    Ok(())
}

async fn inspect(
    client: &Client,
    target: String,
    grant_file: Option<PathBuf>,
    local_addresses: Vec<std::net::SocketAddr>,
) -> anyhow::Result<(DeviceConfig, DeviceView)> {
    let grant = read_grant(grant_file.clone()).await?;
    let Reply::Connected(session) = client
        .request(native::connect_command(
            target.clone(),
            grant,
            local_addresses.clone(),
        ))
        .await?
    else {
        anyhow::bail!("unexpected connection response");
    };
    let caps = super::wait_for_displays(client, session).await?;
    let authenticated = client
        .snapshot()
        .await?
        .sessions
        .into_iter()
        .find(|entry| entry.id == session)
        .context("inspected computer disconnected")?
        .peer;
    let pinned = native::reconnect_target(&target, authenticated)?;
    let has_monitors = caps.displays.iter().any(|d| d.index & (1 << 31) != 0);
    let displays = caps
        .displays
        .into_iter()
        .filter(|d| !has_monitors || d.index & (1 << 31) != 0)
        .take(32)
        .collect();
    let label = rds_net::parse_target(&target)
        .map(|a| format!("Device {}", &a.id.to_string()[..8]))
        .unwrap_or_else(|_| target.chars().take(64).collect());
    let device = DeviceConfig {
        key: target.clone(),
        target: pinned,
        label: label.clone(),
        grant_file,
        local_addresses,
    };
    Ok((
        device,
        DeviceView {
            key: target,
            label,
            displays,
        },
    ))
}

async fn actor(
    client: Client,
    mut config: WorkspaceConfig,
    path: PathBuf,
    options: Options,
    view: WorkspaceHandle,
    mut events: tokio::sync::mpsc::Receiver<WorkspaceEvent>,
    stop: CancellationToken,
) {
    let mut sessions = BTreeMap::<TabId, SessionTask>::new();
    let mut bindings = BTreeMap::<String, native::PeerBinding>::new();
    let mut inspections = tokio::task::JoinSet::<anyhow::Result<(DeviceConfig, DeviceView)>>::new();
    if let Ok(Ok(snapshot)) = tokio::time::timeout(Duration::from_secs(2), client.snapshot()).await
    {
        for session in snapshot.sessions {
            if config.devices.len() >= 32 {
                break;
            }
            if config
                .devices
                .iter()
                .any(|device| device.targets_peer(&session.peer))
            {
                continue;
            }
            let label = format!(
                "Device {}",
                session.peer.chars().take(8).collect::<String>()
            );
            let device = DeviceConfig {
                key: session.peer.clone(),
                target: session.peer,
                label: label.clone(),
                grant_file: None,
                local_addresses: vec![],
            };
            let _ = view.update(WorkspaceUpdate::Device(DeviceView {
                key: device.key.clone(),
                label,
                displays: vec![],
            }));
            config.devices.push(device);
        }
    }
    let deadline = async {
        if let Some(seconds) = options.duration {
            tokio::time::sleep(Duration::from_secs(seconds.get())).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = &mut deadline => { let _ = view.update(WorkspaceUpdate::CloseWindow); break; },
            result = inspections.join_next(), if !inspections.is_empty() => {
                match result {
                    Some(Ok(Ok((device, inventory)))) => {
                        if let Some(old) = config.devices.iter_mut().find(|d| d.key == device.key) {
                            if old.grant_file != device.grant_file {
                                let _ = view.update(WorkspaceUpdate::Message("This device already has a different authorization profile".into()));
                                continue;
                            }
                            *old = device;
                        } else if config.devices.len() < 32 { config.devices.push(device); }
                        else { let _ = view.update(WorkspaceUpdate::Message("At most 32 saved computers are supported".into())); continue; }
                        let _ = view.update(WorkspaceUpdate::Device(inventory));
                    }
                    Some(Ok(Err(error))) => { let _ = view.update(WorkspaceUpdate::Message(format!("Could not load displays: {error}"))); }
                    Some(Err(_)) => { let _ = view.update(WorkspaceUpdate::Message("Display lookup ended unexpectedly".into())); }
                    None => {}
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break; };
                match event {
                    WorkspaceEvent::Start { tab, view: display, mut input } => {
                        if let Some(previous) = sessions.remove(&tab.id) { previous.finish().await; }
                        let Some(device) = config.devices.iter().find(|device| device.key == tab.spec.device).cloned() else {
                            display.status("Device profile unavailable"); continue;
                        };
                        let session_stop = stop.child_token();
                        let worker_stop = session_stop.clone();
                        let mut selected = options.clone();
                        selected.display = tab.spec.display;
                        selected.max_fps = std::num::NonZeroU32::new(tab.spec.profile.max_fps).expect("validated tab profile");
                        selected.clipboard = tab.spec.profile.clipboard;
                        selected.payload_receipts = tab.spec.profile.payload_receipts;
                        selected.resolution = match tab.spec.profile.video_size {
                            VideoSize::Hd => Resolution::Hd, VideoSize::FullHd => Resolution::FullHd, VideoSize::Native => Resolution::Native,
                        };
                        let peer_client = client.clone();
                        let binding = bindings.entry(device.key.clone()).or_default().clone();
                        let task = tokio::spawn(async move {
                            let diagnostic_stop = worker_stop.child_token();
                            let observing = native::observe(display.clone(), selected.diagnostics_dir.clone(),
                                selected.diagnostic_health.clone(), diagnostic_stop.clone(), Some((tab.id.value(), tab.revision)));
                            let running = async {
                                let result = async {
                                let grant = read_grant(device.grant_file).await?;
                                native::network(native::Source::Managed { client: peer_client, session: None,
                                    peer: device.target, grant, binding, local_addresses: device.local_addresses }, &selected, &display, &mut input, worker_stop).await
                                }.await;
                                if let Err(error) = &result { display.status(format!("Disconnected: {error}")); }
                                diagnostic_stop.cancel();
                                result
                            };
                            let (result, _) = tokio::join!(running, observing);
                            result
                        });
                        sessions.insert(tab.id, SessionTask { stop: session_stop, task });
                    }
                    WorkspaceEvent::Stop(id) => { if let Some(session) = sessions.remove(&id) { session.finish().await; } }
                    WorkspaceEvent::Inspect { target, grant_file } => {
                        if inspections.len() >= 4 { let _ = view.update(WorkspaceUpdate::Message("Display lookup is busy; try again".into())); continue; }
                        let client = client.clone();
                        let grant_file = if grant_file.is_empty() {
                            config.devices.iter().find(|device| device.key == target).and_then(|device| device.grant_file.clone())
                        } else { Some(PathBuf::from(grant_file)) };
                        if config.devices.iter().any(|device| device.key == target && device.grant_file != grant_file) {
                            let _ = view.update(WorkspaceUpdate::Message("This device already has a different authorization profile".into()));
                            continue;
                        }
                        let key = target.clone();
                        let original = config.devices.iter().find(|device| device.key == key)
                            .map_or(target.as_str(), |device| device.target.as_str());
                        let target = bindings.get(&key).map_or_else(|| original.to_owned(), |binding| binding.target(original));
                        let local_addresses = config.devices.iter().find(|device| device.key == key)
                            .map(|device| device.local_addresses.clone()).unwrap_or_default();
                        inspections.spawn(async move {
                            let (mut device, mut inventory) = tokio::time::timeout(Duration::from_secs(20), inspect(&client, target, grant_file, local_addresses)).await
                                .context("display lookup timed out")??;
                            device.key = key.clone(); inventory.key = key;
                            Ok((device, inventory))
                        });
                    }
                    WorkspaceEvent::Save { tabs } => {
                        config.tabs = tabs;
                        for device in &mut config.devices {
                            if let Some(binding) = bindings.get(&device.key) { device.target = binding.target(&device.target); }
                        }
                        let saved = config.clone();
                        let destination = path.clone();
                        let result = tokio::task::spawn_blocking(move || store::save(&destination, &saved)).await;
                        let message = match result {
                            Ok(Ok(())) => "Workspace saved".into(),
                            Ok(Err(error)) => format!("Could not save workspace: {error}"),
                            Err(_) => "Workspace save ended unexpectedly".into(),
                        };
                        let _ = view.update(WorkspaceUpdate::Message(message));
                    }
                }
            }
        }
    }
    inspections.shutdown().await;
    for session in sessions.values() {
        session.stop.cancel();
    }
    let mut closing = tokio::task::JoinSet::new();
    for (_, session) in sessions {
        closing.spawn(session.finish());
    }
    while closing.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_peer_discovery_keeps_the_existing_ticket_profile() {
        let peer = rds_net::SecretKey::from_bytes(&[41; 32]).public();
        let other = rds_net::SecretKey::from_bytes(&[42; 32]).public();
        let address =
            rds_net::EndpointAddr::new(peer).with_ip_addr("127.0.0.1:9000".parse().unwrap());
        let device = DeviceConfig {
            key: "saved-profile".into(),
            target: rds_net::Ticket(address.clone()).to_string(),
            label: "Work computer".into(),
            grant_file: Some("external-grant.json".into()),
            local_addresses: vec![],
        };
        assert!(device.targets_peer(&peer.to_string()));
        assert!(!device.targets_peer(&other.to_string()));
        assert_eq!(rds_net::parse_target(&device.target).unwrap(), address);
        assert_eq!(device.grant_file, Some("external-grant.json".into()));

        let alternate_route = DeviceConfig {
            target: rds_net::Ticket(address.with_ip_addr("127.0.0.1:9001".parse().unwrap()))
                .to_string(),
            ..device.clone()
        };
        assert!(alternate_route.targets_peer(&peer.to_string()));

        let bare = DeviceConfig {
            target: peer.to_string(),
            ..device.clone()
        };
        assert!(bare.targets_peer(&peer.to_string()));
        assert!(!bare.targets_peer(&other.to_string()));
        for target in ["named-computer", "rds1invalid-ticket", ""] {
            let unresolved = DeviceConfig {
                target: target.into(),
                ..device.clone()
            };
            assert!(!unresolved.targets_peer(&peer.to_string()));
        }
    }
}
