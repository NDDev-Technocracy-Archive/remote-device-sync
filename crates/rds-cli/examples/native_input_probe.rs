//! Bounded native UI qualification through an existing admitted local agent.
//! Sends only explicit fixture focus, F8 and copy/paste/cut/select/save/close
//! chords. Never loads a key, enrolls a peer, changes consent or reads/stores
//! clipboard contents or screen pixels. Both input and output require owned pipes.
use anyhow::{Context, ensure};
use clap::{Parser, ValueEnum};
use rds_client::local::{Client, ManagedMessage};
use rds_core::{Codec, DesktopEvent, DesktopHello, InputKind, local::SessionId};
use std::os::fd::AsFd;
use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, ValueEnum)]
enum Modifier {
    Control,
    Command,
}
impl Modifier {
    fn key(self) -> u32 {
        match self {
            Self::Control => 29,
            Self::Command => 125,
        }
    }
}

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_directory: PathBuf,
    /// An existing admitted route; never enroll a new identity for this probe.
    #[arg(long)]
    target_ticket: PathBuf,
    #[arg(long)]
    display: u32,
    /// Fraction of the admitted display containing the owned fixture point.
    #[arg(long, requires = "focus_y")]
    focus_x: Option<f64>,
    #[arg(long, requires = "focus_x")]
    focus_y: Option<f64>,
    #[arg(long, default_value_t = 45)]
    duration: u64,
    /// Native shortcut modifier of the explicitly selected fixture device.
    #[arg(long, value_enum, default_value = "control")]
    modifier: Modifier,
}

async fn emit(
    output: &mut tokio::net::unix::pipe::Sender,
    value: serde_json::Value,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    ensure!(
        bytes.len() < 8192,
        "fixture response exceeds its metadata bound"
    );
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    ensure!((5..=120).contains(&args.duration), "invalid probe duration");
    let point = args.focus_x.zip(args.focus_y);
    if let Some((x, y)) = point {
        ensure!(
            x.is_finite() && y.is_finite() && (0.0..1.0).contains(&x) && (0.0..1.0).contains(&y),
            "invalid owned fixture point"
        );
    }
    let client = Client::new(args.control_directory);
    let ticket = tokio::task::spawn_blocking(move || {
        use rustix::fs::{Mode, OFlags};
        let file = std::fs::File::from(rustix::fs::open(
            &args.target_ticket,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        ensure!(
            file.metadata()?.is_file(),
            "fixture route must be a regular file"
        );
        let mut ticket = String::new();
        file.take(8193).read_to_string(&mut ticket)?;
        Ok::<_, anyhow::Error>(ticket)
    })
    .await??;
    ensure!(ticket.len() <= 8192, "oversized fixture route");
    let target: rds_net::Ticket = ticket
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("fixture route must be a valid pinned ticket"))?;
    let before = client.snapshot().await?;
    let session = before
        .sessions
        .iter()
        .find(|row| {
            row.peer == target.0.id.to_string() && row.status == rds_core::local::Status::Connected
        })
        .map(|row| row.id)
        .context("fixture requires an already connected, admitted manager session")?;
    let output = std::io::stdout().as_fd().try_clone_to_owned()?;
    let output = tokio::net::unix::pipe::Sender::from_owned_fd(output)?;
    tokio::time::timeout(
        Duration::from_secs(args.duration),
        run(
            &client,
            session,
            args.display,
            point,
            args.modifier.key(),
            output,
        ),
    )
    .await
    .context("owned input probe expired")?
}

async fn run(
    client: &Client,
    session: SessionId,
    display: u32,
    point: Option<(f64, f64)>,
    modifier: u32,
    mut output: tokio::net::unix::pipe::Sender,
) -> anyhow::Result<()> {
    let (mut desktop, mut events) = client
        .desktop_features(
            Some(session),
            DesktopHello {
                display,
                max_fps: 5,
                codec: Codec::H264,
                input_acks: true,
            },
            360,
            false,
            false,
        )
        .await?;
    let admitted = desktop
        .caps
        .displays
        .iter()
        .find(|row| row.index == display)
        .context("fixture display not admitted")?;
    ensure!(
        admitted.width > 0 && admitted.height > 0,
        "invalid admitted display extent"
    );
    let point = point.map(|(x, y)| {
        (
            x * f64::from(admitted.width),
            y * f64::from(admitted.height),
        )
    });
    emit(
        &mut output,
        serde_json::json!({"ready":true,"displays":desktop.caps.displays}),
    )
    .await?;
    // A pipe-specific owned receiver validates its FD and sets nonblocking I/O.
    // Unlike Tokio's blocking stdin reader, expiry never waits for an operator.
    let input = std::io::stdin().as_fd().try_clone_to_owned()?;
    let mut input =
        tokio::io::BufReader::new(tokio::net::unix::pipe::Receiver::from_owned_fd(input)?);
    let mut pending = BTreeMap::new();
    let mut frames = 0u64;
    loop {
        let command = command(&mut input);
        tokio::pin!(command);
        let command = loop {
            tokio::select! {
                message = desktop.recv() => match message? {
                    Some(ManagedMessage::Frame(_)) => { frames += 1; },
                    Some(ManagedMessage::Event(_)) => anyhow::bail!("events did not retain their separate route"),
                    None => anyhow::bail!("fixture desktop ended"),
                },
                event = events.recv() => {
                    let event = event.context("fixture event route ended")??;
                    if let DesktopEvent::InputAck {seq,..} = event
                        && let Some(started) = pending.remove(&seq) {
                            let started: Instant = started;
                            emit(&mut output, serde_json::json!({"ack":seq,"elapsed_us":started.elapsed().as_micros(),"remaining":pending.len()})).await?;
                        }
                },
                result = &mut command => break result?,
            }
        };
        match command {
            Some(Command::Shortcut(key)) => {
                ensure!(
                    pending.is_empty(),
                    "previous fixture gesture is still pending"
                );
                let started = Instant::now();
                for kind in [
                    InputKind::KeyDown { code: modifier },
                    InputKind::KeyDown { code: key },
                    InputKind::KeyUp { code: key },
                    InputKind::KeyUp { code: modifier },
                ] {
                    let seq = desktop.send_input(kind).await?;
                    pending.insert(seq, started);
                }
            }
            Some(Command::F8) => {
                ensure!(
                    pending.is_empty(),
                    "previous fixture gesture is still pending"
                );
                let started = Instant::now();
                for kind in [
                    InputKind::KeyDown { code: 66 },
                    InputKind::KeyUp { code: 66 },
                ] {
                    let seq = desktop.send_input(kind).await?;
                    pending.insert(seq, started);
                }
            }
            Some(Command::Point) => {
                ensure!(
                    pending.is_empty(),
                    "previous fixture gesture is still pending"
                );
                let (x, y) = point.context("point requires an explicit owned fixture point")?;
                let started = Instant::now();
                let seq = desktop.send_input(InputKind::PointerMove { x, y }).await?;
                pending.insert(seq, started);
            }
            Some(Command::Focus) => {
                ensure!(
                    pending.is_empty(),
                    "previous fixture gesture is still pending"
                );
                ensure!(
                    point.is_some(),
                    "focus requires an explicit owned fixture point"
                );
                let started = Instant::now();
                for kind in [
                    InputKind::PointerButton {
                        button: 0x110,
                        pressed: true,
                    },
                    InputKind::PointerButton {
                        button: 0x110,
                        pressed: false,
                    },
                ] {
                    let seq = desktop.send_input(kind).await?;
                    pending.insert(seq, started);
                }
            }
            Some(Command::Finish) => {
                ensure!(
                    pending.is_empty(),
                    "fixture input acknowledgement is incomplete"
                );
                desktop.finish().await?;
                emit(
                    &mut output,
                    serde_json::json!({"finished":true,"received_frames":frames}),
                )
                .await?;
                return Ok(());
            }
            None => anyhow::bail!("fixture operator departed"),
        }
    }
}

enum Command {
    Shortcut(u32),
    Point,
    Focus,
    F8,
    Finish,
}

async fn command(
    input: &mut tokio::io::BufReader<tokio::net::unix::pipe::Receiver>,
) -> anyhow::Result<Option<Command>> {
    let mut bytes = String::with_capacity(7);
    // The caller retains this future across media/events; a partial command
    // cannot be canceled and interpreted as a replacement command's bytes.
    let length = input.take(7).read_line(&mut bytes).await?;
    match (length, bytes.as_str()) {
        (0, _) => Ok(None),
        (_, "copy\n") => Ok(Some(Command::Shortcut(46))),
        (_, "paste\n") => Ok(Some(Command::Shortcut(47))),
        (_, "cut\n") => Ok(Some(Command::Shortcut(45))),
        (_, "select\n") => Ok(Some(Command::Shortcut(30))),
        (_, "save\n") => Ok(Some(Command::Shortcut(31))),
        (_, "close\n") => Ok(Some(Command::Shortcut(17))),
        (_, "F8\n") => Ok(Some(Command::F8)),
        (_, "focus\n") => Ok(Some(Command::Focus)),
        (_, "point\n") => Ok(Some(Command::Point)),
        (_, "finish\n") => Ok(Some(Command::Finish)),
        _ => anyhow::bail!(
            "unknown fixture command; use point, focus, F8, copy, paste, cut, select, save, close or finish"
        ),
    }
}
