#![allow(dead_code)]
use rds_ssh::{Authentication, Config, PrivateKey};
use russh::{Channel, ChannelId, Pty, server};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub fn key(seed: u8) -> PrivateKey {
    PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &[seed; 32],
    ))
}

pub fn config() -> Config {
    Config {
        user: "fixture".into(),
        host_key: key(1).public_key().clone(),
        authentication: Authentication::Key(Arc::new(key(2))),
    }
}

#[derive(Clone, Copy)]
pub enum Case {
    Echo,
    EarlyExit,
    MissingStatus,
    RejectAuth,
    RejectPty,
    RejectExec,
    Resize,
    Hold,
    SpoofTelemetry,
}

pub const SPOOF: &[u8] = b"{\"schema_version\":1,\"timestamp_unix_us\":1790300000000000,\"uptime_us\":100,\"service\":\"rds-cli\",\"version\":\"0.1.0\",\"run_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"sequence\":1,\"level\":\"INFO\",\"target\":\"PRIVATE_SENTINEL\",\"line\":1,\"session_id\":null,\"event\":\"process_started\",\"operation\":null,\"outcome\":null,\"elapsed_us\":null,\"telemetry_dropped_total\":0,\"telemetry_oversize_total\":0,\"telemetry_write_errors_total\":0}\n";

#[derive(Default)]
pub struct Observed {
    pub auth: AtomicUsize,
    pub exec: AtomicUsize,
    pub pty: AtomicUsize,
    pub resized: AtomicUsize,
}

pub struct Handler {
    pub case: Case,
    pub observed: Arc<Observed>,
}

impl server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        self.observed.auth.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == "fixture"
                && key.public_key.key_data() == self::key(2).public_key().key_data()
                && !matches!(self.case, Case::RejectAuth)
            {
                server::Auth::Accept
            } else {
                server::Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                }
            },
        )
    }
    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        width: u32,
        height: u32,
        _: u32,
        _: u32,
        modes: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(term, "xterm-256color");
        assert!(width > 0 && height > 0);
        assert!(modes.contains(&(Pty::IUTF8, 1)));
        self.observed.pty.fetch_add(1, Ordering::SeqCst);
        if matches!(self.case, Case::RejectPty) {
            session.channel_failure(channel)?;
        } else {
            session.channel_success(channel)?;
        }
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.exec_request(channel, b"fixture", session).await
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(command, b"fixture");
        self.observed.exec.fetch_add(1, Ordering::SeqCst);
        if matches!(self.case, Case::RejectExec) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        match self.case {
            Case::SpoofTelemetry => {
                session.extended_data(channel, 1, SPOOF.to_vec())?;
                session.exit_status_request(channel, 0)?;
                session.eof(channel)?;
                session.close(channel)?;
            }
            Case::EarlyExit => {
                session.exit_status_request(channel, 23)?;
                session.data(channel, b"after-status\n".to_vec())?;
                session.extended_data(channel, 1, b"stderr\n".to_vec())?;
                session.eof(channel)?;
                session.close(channel)?;
            }
            Case::MissingStatus => {
                session.eof(channel)?;
                session.close(channel)?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        Ok(())
    }
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if matches!(self.case, Case::Echo) {
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        width: u32,
        height: u32,
        _: u32,
        _: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if matches!(self.case, Case::Resize) {
            assert_eq!((width, height), (120, 40));
            self.observed.resized.fetch_add(1, Ordering::SeqCst);
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }
}

pub fn server_config() -> Arc<server::Config> {
    Arc::new(server::Config {
        keys: vec![key(1)],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(90)),
        ..Default::default()
    })
}

pub fn fixture(
    case: Case,
) -> (
    tokio::io::DuplexStream,
    Arc<Observed>,
    tokio::task::JoinHandle<()>,
) {
    let (client, remote) = tokio::io::duplex(4096);
    let observed = Arc::new(Observed::default());
    let handler = Handler {
        case,
        observed: observed.clone(),
    };
    let task = tokio::spawn(async move {
        if let Ok(session) = server::run_stream(server_config(), remote, handler).await {
            let _ = session.await;
        }
    });
    (client, observed, task)
}

pub async fn finished(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("server did not observe transport closure")
        .unwrap();
}
