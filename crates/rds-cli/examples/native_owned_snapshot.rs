//! Explicit owned-window QA images through an existing admitted remote host.
//! No endpoint key, new connection, OS permission, clipboard or input access.
use anyhow::{Context, ensure};
use clap::Parser;
use rds_client::local::{Client, ManagedMessage};
use rds_core::{Codec, DesktopHello, local::Status};
use rds_desktop::{
    RawFrame,
    client::{RelayDecoder, RelayOutcome},
};
use std::{io::Write, path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    control_directory: PathBuf,
    #[arg(long)]
    peer: rds_net::EndpointId,
    #[arg(long)]
    display: u32,
    #[arg(long)]
    x: u32,
    #[arg(long)]
    y: u32,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
    /// A new private PNG of only the operator's owned fixture rectangle.
    #[arg(long)]
    output: PathBuf,
    /// Also retain the following decoded crop for startup freshness diagnosis.
    #[arg(long)]
    next_output: Option<PathBuf>,
}

#[derive(Clone, Copy)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
impl Crop {
    fn validate(self) -> anyhow::Result<Self> {
        ensure!(
            self.width > 0
                && self.height > 0
                && u64::from(self.width) * u64::from(self.height) <= 4096 * 4096,
            "invalid owned image extent"
        );
        ensure!(
            self.x.checked_add(self.width).is_some() && self.y.checked_add(self.height).is_some(),
            "owned image coordinates overflow"
        );
        Ok(self)
    }
    fn write(self, frame: RawFrame, path: PathBuf) -> anyhow::Result<()> {
        ensure!(
            self.x + self.width <= frame.width && self.y + self.height <= frame.height,
            "owned crop escapes the received display"
        );
        ensure!(
            u64::from(frame.width) * 4 <= u64::from(frame.stride)
                && u64::from(frame.stride) * u64::from(frame.height) <= frame.data.len() as u64,
            "invalid received pixel layout"
        );
        let fd = rustix::fs::open(
            &path,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        let file = std::fs::File::from(fd);
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut stream = writer.stream_writer()?;
        let mut row = vec![0u8; self.width as usize * 4];
        for y in self.y..self.y + self.height {
            let start = y as usize * frame.stride as usize + self.x as usize * 4;
            let pixels = &frame.data[start..start + row.len()];
            for (output, input) in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(pixels.as_chunks::<4>().0)
            {
                output.copy_from_slice(&[input[2], input[1], input[0], 255]);
            }
            stream.write_all(&row)?;
        }
        stream.finish()?;
        writer.finish()?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let crop = Crop {
        x: args.x,
        y: args.y,
        width: args.width,
        height: args.height,
    }
    .validate()?;
    tokio::time::timeout(Duration::from_secs(20), capture(args, crop))
        .await
        .context("owned snapshot expired")?
}

async fn capture(args: Args, crop: Crop) -> anyhow::Result<()> {
    let client = Client::new(args.control_directory);
    let snapshot = client.snapshot().await?;
    let sessions = snapshot
        .sessions
        .iter()
        .filter(|row| row.peer == args.peer.to_string() && row.status == Status::Connected)
        .collect::<Vec<_>>();
    ensure!(
        sessions.len() == 1,
        "snapshot requires exactly one already admitted connected peer"
    );
    let (mut desktop, mut events) = client
        .desktop_features(
            Some(sessions[0].id),
            DesktopHello {
                display: args.display,
                max_fps: 2,
                codec: Codec::H264,
                input_acks: false,
            },
            0,
            false,
            false,
        )
        .await?;
    ensure!(
        desktop
            .caps
            .displays
            .iter()
            .any(|d| d.index == args.display),
        "snapshot display not admitted"
    );
    ensure!(
        args.next_output.as_ref() != Some(&args.output),
        "snapshot outputs must differ"
    );
    let mut outputs = std::collections::VecDeque::from([args.output]);
    if let Some(next) = args.next_output {
        outputs.push_back(next);
    }
    let mut captures = Vec::with_capacity(outputs.len());
    let mut decoder = RelayDecoder::new();
    loop {
        tokio::select! {
            event=events.recv()=>{event.context("owned event route ended")??;},
            message=desktop.recv()=> {
                match message?.context("owned media route ended")? {
                    ManagedMessage::Event(_)=>anyhow::bail!("owned events lost their separate route"),
                    ManagedMessage::Frame(frame)=> {
                        let header=frame.header.clone();
                        let (next,outcome)=decoder.push_bounded(frame.header,frame.payload).await?;
                        decoder=next;
                        match outcome {
                            RelayOutcome::NeedIdr=>desktop.request_idr().await?,
                            RelayOutcome::Frame(raw)=> {
                                let received=(raw.width,raw.height);
                                let output=outputs.pop_front().context("snapshot output owner ended")?;
                                tokio::task::spawn_blocking(move ||crop.write(raw,output)).await??;
                                captures.push(serde_json::json!({"header":header,"received_width":received.0,"received_height":received.1,"crop":{"x":crop.x,"y":crop.y,"width":crop.width,"height":crop.height}}));
                                if outputs.is_empty() {
                                    desktop.finish().await?;
                                    println!("{}",serde_json::json!({"captured":true,"captures":captures}));
                                    return Ok(());
                                }
                            }
                            RelayOutcome::Pending|RelayOutcome::Discarded=>{},
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_contains_only_the_owned_crop_with_correct_pixel_channels_and_stride() {
        use std::os::unix::fs::DirBuilderExt;
        let dir =
            std::env::temp_dir().join(format!("nddev-owned-crop-{:032x}", rand::random::<u128>()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let crop = Crop {
            x: 1,
            y: 0,
            width: 1,
            height: 2,
        }
        .validate()
        .unwrap();
        let frame = || RawFrame {
            width: 2,
            height: 2,
            stride: 12,
            data: vec![
                0, 0, 0, 255, 0, 0, 255, 255, 7, 7, 7, 7, 0, 0, 0, 255, 0, 255, 0, 255, 9, 9, 9, 9,
            ]
            .into(),
        };
        let path = dir.join("own.png");
        crop.write(frame(), path.clone()).unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
                .read_info()
                .unwrap();
        assert_eq!((reader.info().width, reader.info().height), (1, 2));
        let mut pixels = [0; 8];
        reader.next_frame(&mut pixels).unwrap();
        reader.finish().unwrap();
        assert_eq!(pixels, [255, 0, 0, 255, 0, 255, 0, 255]);
        assert!(crop.write(frame(), path.clone()).is_err());
        let invalid = Crop { x: 2, ..crop };
        let missing = dir.join("outside.png");
        assert!(invalid.write(frame(), missing.clone()).is_err());
        assert!(!missing.exists());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
