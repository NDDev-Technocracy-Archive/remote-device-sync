//! Standalone native application entry point. Uses the local agent and never
//! loads an endpoint key or creates a second network identity.
use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use rds_client::local::Client;
use rds_core::local::{Command, Reply};
use std::{num::NonZeroU32, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "NDDev Remote — native remote screens")]
struct Cli {
    target: Vec<String>,
    #[arg(long)]
    control_dir: Option<PathBuf>,
    #[arg(long)]
    grant_file: Option<PathBuf>,
    /// Open displays in separate native windows instead of workspace tabs.
    #[arg(long)]
    separate_windows: bool,
    /// One native fullscreen window per monitor; macOS three-finger Spaces swipe.
    #[arg(long, conflicts_with = "separate_windows")]
    spaces: bool,
    /// Use a single window with tabs, overriding the saved presentation.
    #[arg(long, conflicts_with_all = ["spaces", "separate_windows"])]
    tabs: bool,
    /// Saved workspace preferences; an explicit path isolates this workspace.
    #[arg(long)]
    workspace_file: Option<PathBuf>,
    /// Open one independent window for every available monitor.
    #[arg(long, conflicts_with_all = ["headless", "report", "diagnostic_visual_probe"])]
    all_displays: bool,
    /// List display IDs and geometry without opening a viewer.
    #[arg(long, conflicts_with = "all_displays")]
    list_displays: bool,
    #[command(flatten)]
    options: rds_cli::desktop::Options,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionConfig {
    target: String,
    #[serde(default)]
    displays: Vec<u32>,
    grant_file: Option<PathBuf>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    schema_version: u32,
    connections: Vec<ConnectionConfig>,
    target: Option<String>,
    display: Option<u32>,
    max_fps: Option<NonZeroU32>,
    grant_file: Option<PathBuf>,
    resolution: Option<rds_cli::desktop::Resolution>,
    payload_receipts: Option<bool>,
    clipboard: Option<bool>,
    diagnostic_visual_probe: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).expect("validated CLI arguments");
    let setup = (|| -> anyhow::Result<_> {
        let (log, directory) = rds_cli::logging::ViewerLog::create()?;
        cli.options.diagnostics_dir = Some(directory);
        Ok(rds_observe::install_with_writer(
            rds_observe::Service::Cli,
            rds_observe::Config::from_env("warn,rds_cli=info,rds_desktop=info")?,
            log,
        )?)
    })();
    match setup {
        Ok(telemetry) => {
            cli.options.diagnostic_health = Some(telemetry.health_observer());
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                tracing::error!(panic=%info,"native viewer panic");
                previous(info);
            }));
            tracing::info!(
                pid = std::process::id(),
                "native viewer started with persistent diagnostics"
            );
            let result = telemetry.run(run(cli, &matches)).await;
            telemetry.finish(result)
        }
        Err(error) => {
            eprintln!("could not initialize viewer diagnostics: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(mut cli: Cli, matches: &clap::ArgMatches) -> anyhow::Result<()> {
    let explicit_targets = !cli.target.is_empty();
    let mut configured_resolution = None;
    let mut connections = Vec::new();
    let key_path = rds_net::default_key_path();
    let directory = match cli.control_dir.take() {
        Some(directory) => directory,
        None => rds_client::local::control_dir_for_key(
            key_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("no local agent configuration directory"))?,
        )?,
    };
    if let Some(key_path) = key_path
        && cli.workspace_file.is_none()
    {
        let config_path = key_path.with_file_name("viewer.json");
        let read = (|| -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let file = std::fs::File::from(rustix::fs::open(
                &config_path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?);
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other(
                    "viewer configuration must be a regular file",
                ));
            }
            let mut bytes = Vec::new();
            file.take(16385).read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        match read {
            Ok(bytes) => {
                anyhow::ensure!(bytes.len() <= 16384, "viewer configuration exceeds 16 KiB");
                let config: Config = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(
                    config.schema_version == 1,
                    "unsupported viewer configuration version"
                );
                if cli.target.is_empty() {
                    if config.connections.is_empty() {
                        cli.target.extend(config.target);
                    } else {
                        connections = config.connections;
                    }
                }
                if let Some(display) = config.display
                    && matches.value_source("display") != Some(ValueSource::CommandLine)
                {
                    cli.options.display = display;
                }
                if let Some(fps) = config.max_fps
                    && matches.value_source("max_fps") != Some(ValueSource::CommandLine)
                {
                    cli.options.max_fps = fps;
                }
                if let Some(enabled) = config.payload_receipts
                    && matches.value_source("payload_receipts") != Some(ValueSource::CommandLine)
                {
                    cli.options.payload_receipts = enabled;
                }
                if let Some(enabled) = config.clipboard
                    && matches.value_source("clipboard") != Some(ValueSource::CommandLine)
                {
                    cli.options.clipboard = enabled;
                }
                if let Some(probe) = config.diagnostic_visual_probe
                    && matches.value_source("diagnostic_visual_probe")
                        != Some(ValueSource::CommandLine)
                {
                    cli.options.diagnostic_visual_probe = Some(probe);
                }
                if cli.grant_file.is_none() {
                    cli.grant_file = config.grant_file;
                }
                configured_resolution = config.resolution;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !cli.separate_windows
        && !cli.options.headless
        && !cli.list_displays
        && cli.options.report.is_none()
        && cli.options.diagnostic_visual_probe.is_none()
    {
        use rds_cli::desktop::workspace::{DeviceConfig, Presentation, WorkspaceConfig};
        use rds_desktop::render::workspace::{TabProfile, TabSpec, VideoSize};
        if matches.value_source("resolution") != Some(ValueSource::CommandLine)
            && let Some(resolution) = configured_resolution
        {
            cli.options.resolution = resolution;
        }
        let path = cli.workspace_file.clone().unwrap_or_else(|| {
            directory
                .parent()
                .unwrap_or(&directory)
                .join("viewer-workspace.json")
        });
        if !explicit_targets && let Some(mut saved) = WorkspaceConfig::load(path.clone()).await? {
            if cli.spaces {
                saved.presentation = Presentation::Spaces;
            }
            if cli.tabs {
                saved.presentation = Presentation::Tabs;
            }
            return rds_cli::desktop::workspace::run(
                Client::new(&directory),
                saved,
                path,
                cli.options,
                cli.all_displays,
            )
            .await;
        }
        if connections.is_empty() {
            connections = cli
                .target
                .iter()
                .map(|target| ConnectionConfig {
                    target: target.clone(),
                    displays: vec![cli.options.display],
                    grant_file: cli.grant_file.clone(),
                })
                .collect();
        }
        let windows = window_plan(connections, cli.options.display)?;
        let mut devices = Vec::<DeviceConfig>::new();
        let mut tabs = vec![];
        for window in windows {
            let label = rds_net::parse_target(&window.target)
                .map(|a| format!("Device {}", &a.id.to_string()[..8]))
                .unwrap_or_else(|_| window.target.chars().take(64).collect());
            if !devices.iter().any(|d| d.key == window.target) {
                devices.push(DeviceConfig {
                    key: window.target.clone(),
                    target: window.target.clone(),
                    label: label.clone(),
                    grant_file: window.grant_file.or_else(|| cli.grant_file.clone()),
                    local_addresses: vec![],
                });
            }
            tabs.push(TabSpec {
                device: window.target,
                label,
                display: window.display,
                profile: TabProfile {
                    video_size: match cli.options.resolution {
                        rds_cli::desktop::Resolution::Hd => VideoSize::Hd,
                        rds_cli::desktop::Resolution::FullHd => VideoSize::FullHd,
                        rds_cli::desktop::Resolution::Native => VideoSize::Native,
                    },
                    max_fps: cli.options.max_fps.get(),
                    clipboard: cli.options.clipboard,
                    interactive: true,
                    payload_receipts: cli.options.payload_receipts,
                },
            });
        }
        // An empty launch opens the connection picker; the local manager remains
        // the only endpoint identity and connection owner.
        let config = WorkspaceConfig {
            schema_version: 1,
            presentation: if cli.spaces {
                Presentation::Spaces
            } else {
                Presentation::Tabs
            },
            devices,
            tabs,
        };
        return rds_cli::desktop::workspace::run(
            Client::new(&directory),
            config,
            path,
            cli.options,
            cli.all_displays,
        )
        .await;
    }
    if !cli.list_displays && configure_resolution(&mut cli.options, matches, configured_resolution)
    {
        let Some(height) = rds_desktop::render::choose_resolution() else {
            return Ok(());
        };
        cli.options.resolution = match height {
            720 => rds_cli::desktop::Resolution::Hd,
            0 => rds_cli::desktop::Resolution::Native,
            _ => rds_cli::desktop::Resolution::FullHd,
        };
    }
    let client = Client::new(&directory);
    if connections.is_empty() {
        connections = cli
            .target
            .iter()
            .map(|target| ConnectionConfig {
                target: target.clone(),
                displays: vec![cli.options.display],
                grant_file: cli.grant_file.clone(),
            })
            .collect();
    }
    // Bound launch intent before dialing or fetching per-device inventories.
    let _ = window_plan(connections.clone(), cli.options.display)?;
    if cli.all_displays || cli.list_displays {
        if connections.is_empty() {
            let session = client.selected(None).await?;
            let snapshot = client.snapshot().await?;
            let target = snapshot
                .sessions
                .into_iter()
                .find(|entry| entry.id == session)
                .ok_or_else(|| anyhow::anyhow!("selected device unavailable"))?
                .peer;
            connections.push(ConnectionConfig {
                target,
                displays: vec![],
                grant_file: cli.grant_file.clone(),
            });
        }
        for connection in &mut connections {
            let grant = rds_cli::desktop::read_grant(connection.grant_file.clone()).await?;
            let Reply::Connected(session) = client
                .request(Command::Connect {
                    target: connection.target.clone(),
                    grant,
                })
                .await?
            else {
                anyhow::bail!("unexpected connection response");
            };
            let caps = rds_cli::desktop::wait_for_displays(&client, session).await?;
            if cli.list_displays {
                for display in &caps.displays {
                    println!(
                        "Display {}: {}×{}{}",
                        display.index,
                        display.width,
                        display.height,
                        if display.primary { " (primary)" } else { "" }
                    );
                }
            } else {
                connection.displays = physical_displays(&caps.displays);
            }
        }
        if cli.list_displays {
            return Ok(());
        }
    }
    let windows = window_plan(connections, cli.options.display)?;
    anyhow::ensure!(
        windows.len() <= 1
            || (cli.options.report.is_none() && cli.options.diagnostic_visual_probe.is_none()),
        "presentation reports and visual probes require one explicit window"
    );
    if let Some(first) = windows.first() {
        // Every additional native process shares the local agent identity and
        // gets an explicit peer/display. A different window's Select cannot
        // redirect it; each process owns its own platform event loop.
        for window in windows.iter().skip(1) {
            launch_window(window, &cli.options, &directory)?;
        }
        cli.options.display = first.display;
        let grant = rds_cli::desktop::read_grant(first.grant_file.clone()).await?;
        rds_cli::desktop::managed_target(&client, first.target.clone(), grant, cli.options).await
    } else {
        let grant = rds_cli::desktop::read_grant(cli.grant_file).await?;
        let session = client.selected(None).await?;
        rds_cli::desktop::managed(&client, session, grant, cli.options).await
    }
}

struct WindowRequest {
    target: String,
    display: u32,
    grant_file: Option<PathBuf>,
}
const MAX_WINDOWS: usize = 8;
fn physical_displays(displays: &[rds_core::DisplayInfo]) -> Vec<u32> {
    let physical = displays
        .iter()
        .filter(|d| d.index & (1 << 31) != 0)
        .map(|d| d.index)
        .collect::<Vec<_>>();
    if physical.is_empty() {
        displays.iter().map(|d| d.index).collect()
    } else {
        physical
    }
}
fn window_plan(
    connections: Vec<ConnectionConfig>,
    fallback: u32,
) -> anyhow::Result<Vec<WindowRequest>> {
    let mut windows = Vec::new();
    for connection in connections {
        anyhow::ensure!(
            !connection.target.is_empty() && connection.target.len() <= 8192,
            "invalid connection target"
        );
        let displays = if connection.displays.is_empty() {
            vec![fallback]
        } else {
            connection.displays
        };
        for display in displays {
            if let Some(existing) = windows
                .iter()
                .find(|w: &&WindowRequest| w.target == connection.target && w.display == display)
            {
                anyhow::ensure!(
                    existing.grant_file == connection.grant_file,
                    "duplicate device/display has conflicting grant files"
                );
                continue;
            }
            anyhow::ensure!(
                windows.len() < MAX_WINDOWS,
                "at most eight native windows may be launched together"
            );
            windows.push(WindowRequest {
                target: connection.target.clone(),
                display,
                grant_file: connection.grant_file.clone(),
            });
        }
    }
    Ok(windows)
}
fn launch_window(
    window: &WindowRequest,
    options: &rds_cli::desktop::Options,
    directory: &std::path::Path,
) -> anyhow::Result<()> {
    let resolution = match options.resolution.height() {
        720 => "hd",
        0 => "native",
        _ => "full-hd",
    };
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command.stdin(std::process::Stdio::null());
    command.arg("--separate-windows");
    command
        .arg(&window.target)
        .arg("--control-dir")
        .arg(directory)
        .arg("--display")
        .arg(window.display.to_string())
        .arg("--max-fps")
        .arg(options.max_fps.to_string())
        .arg("--resolution")
        .arg(resolution)
        .arg(format!("--payload-receipts={}", options.payload_receipts))
        .arg(format!("--clipboard={}", options.clipboard));
    if let Some(file) = &window.grant_file {
        command.arg("--grant-file").arg(file);
    }
    if let Some(duration) = options.duration {
        command.arg("--duration").arg(duration.to_string());
    }
    if options.headless {
        command.arg("--headless");
    }
    let mut child = command.spawn()?;
    // Reap children while the native event loop is active. Independent windows
    // keep their own lifetime when the launching window is closed.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Apply explicit quality choices before deciding whether native selection is
/// needed. Command-line values override the persisted profile.
fn configure_resolution(
    options: &mut rds_cli::desktop::Options,
    matches: &clap::ArgMatches,
    configured: Option<rds_cli::desktop::Resolution>,
) -> bool {
    let explicit_cli = matches.value_source("resolution") == Some(ValueSource::CommandLine);
    if !explicit_cli && let Some(resolution) = configured {
        options.resolution = resolution;
    }
    !options.headless && !explicit_cli && configured.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_cli(args: &[&str], configured: Option<&str>) -> (Cli, bool) {
        let matches = Cli::command().try_get_matches_from(args).unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        let config: Config = serde_json::from_str(configured.unwrap_or("{}")).unwrap();
        let picker = configure_resolution(&mut cli.options, &matches, config.resolution);
        (cli, picker)
    }

    #[test]
    fn simultaneous_devices_and_displays_are_explicit_bounded_and_deduplicated() {
        let cli = Cli::try_parse_from([
            "rds-viewer",
            "device-a",
            "device-b",
            "--all-displays",
            "--resolution",
            "full-hd",
        ])
        .unwrap();
        assert_eq!(cli.target, ["device-a", "device-b"]);
        assert!(cli.all_displays);
        let connections: Config = serde_json::from_str(r#"{"connections":[{"target":"device-a","displays":[0,1,1]},{"target":"device-b","displays":[0]}]}"#).unwrap();
        let windows = window_plan(connections.connections, 9).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(
            (windows[0].target.as_str(), windows[0].display),
            ("device-a", 0)
        );
        assert_eq!(
            (windows[2].target.as_str(), windows[2].display),
            ("device-b", 0)
        );
        assert!(
            window_plan(
                vec![ConnectionConfig {
                    target: "device-a".into(),
                    displays: (0..9).collect(),
                    grant_file: None
                }],
                0
            )
            .is_err()
        );
        let conflicting: Config = serde_json::from_str(
            r#"{"connections":[{"target":"device-a","displays":[0],"grant_file":"a.json"},{"target":"device-a","displays":[0],"grant_file":"b.json"}]}"#,
        ).unwrap();
        assert!(window_plan(conflicting.connections, 0).is_err());
    }
    #[test]
    fn all_displays_prefers_logical_monitors_without_duplicating_the_root() {
        let display = |index| rds_core::DisplayInfo {
            index,
            width: 640,
            height: 480,
            primary: false,
        };
        assert_eq!(
            physical_displays(&[display(0), display((1 << 31) + 9), display((1 << 31) + 10)]),
            [(1 << 31) + 9, (1 << 31) + 10]
        );
        assert_eq!(physical_displays(&[display(0), display(1)]), [0, 1]);
    }
    #[test]
    fn persisted_quality_is_applied_without_reopening_the_picker() {
        for (profile, height) in [("hd", 720), ("full-hd", 1080), ("native", 0)] {
            let json = format!(r#"{{"resolution":"{profile}"}}"#);
            let (cli, picker) = configured_cli(&["rds-viewer"], Some(&json));
            assert_eq!(cli.options.resolution.height(), height);
            assert!(
                !picker,
                "a persisted profile must not be overwritten by a picker"
            );
        }
    }

    #[test]
    fn command_line_quality_overrides_the_persisted_profile() {
        let (cli, picker) = configured_cli(
            &["rds-viewer", "--resolution", "native"],
            Some(r#"{"resolution":"hd"}"#),
        );
        assert_eq!(cli.options.resolution.height(), 0);
        assert!(!picker);
    }

    #[test]
    fn only_unconfigured_native_launches_need_the_picker() {
        assert!(configured_cli(&["rds-viewer"], None).1);
        assert!(!configured_cli(&["rds-viewer", "--headless"], None).1);
    }
}
