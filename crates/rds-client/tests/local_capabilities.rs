//! Read-only profile discovery preserves v5 state and does not guess from EOF.
use std::{path::PathBuf, time::Duration};

use rds_client::local::{Client, Error, Prepared, Server, socket};
use rds_core::local::{
    CAPABILITIES_VERSION, Command, ErrorCode, ManagerCapabilities, Reply, Request, Response,
    SessionId, VERSION,
};
use rds_net::{Backend, Endpoint, EndpointConfig, read_frame, write_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        Self(
            std::path::Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join(format!("rds-caps-{:032x}", rand::random::<u128>())),
        )
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

async fn endpoint(backend: Backend) -> Endpoint {
    rds_net::bind_endpoint(EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn mock_probe(
    response: Option<Response>,
    trailing: bool,
) -> Result<Option<ManagerCapabilities>, Error> {
    let root = Root::new();
    let (owner, listener) = socket::SocketOwner::bind(root.0.clone()).await.unwrap();
    let server = tokio::spawn(async move {
        let _owner = owner;
        let (mut stream, _) = listener.accept().await.unwrap();
        socket::authenticate(&stream).unwrap();
        // Frozen v5 decoder's known List request: length, version, enum tag.
        // No newer command variant is sent to find out whether v6 exists.
        let mut request = [0; 6];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [0, 0, 0, 2, 6, 0]);
        if let Some(response) = response {
            write_frame(&mut stream, &response).await.unwrap();
            if trailing {
                stream.write_all(b"x").await.unwrap();
            }
        }
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "capability probe replayed another local request"
        );
    });
    let result = Client::new(&root.0).capabilities().await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn only_the_frozen_v5_version_refusal_means_unsupported() {
    let response = Response {
        version: VERSION,
        result: Err(ErrorCode::Version),
    };
    assert_eq!(postcard::to_stdvec(&response).unwrap(), vec![5, 1, 0]);
    assert!(mock_probe(Some(response), false).await.unwrap().is_none());
}

#[tokio::test]
async fn unavailable_transport_and_incompatible_replies_are_not_version_negotiation() {
    assert!(mock_probe(None, false).await.is_err());
    for version in [VERSION, CAPABILITIES_VERSION] {
        let result = mock_probe(
            Some(Response {
                version,
                result: Err(ErrorCode::Capacity),
            }),
            false,
        )
        .await;
        assert!(matches!(result, Err(Error::Rejected(ErrorCode::Capacity))));
        let result = mock_probe(
            Some(Response {
                version,
                result: Ok(Reply::Done),
            }),
            false,
        )
        .await;
        assert!(matches!(result, Err(Error::Protocol)));
    }
    let result = mock_probe(
        Some(Response {
            version: CAPABILITIES_VERSION,
            result: Err(ErrorCode::Version),
        }),
        false,
    )
    .await;
    assert!(matches!(result, Err(Error::Rejected(ErrorCode::Version))));
    let result = mock_probe(
        Some(Response {
            version: CAPABILITIES_VERSION + 1,
            result: Err(ErrorCode::Version),
        }),
        false,
    )
    .await;
    assert!(matches!(result, Err(Error::Protocol)));
    let result = mock_probe(
        Some(Response {
            version: CAPABILITIES_VERSION,
            result: Ok(Reply::Capabilities(ManagerCapabilities {
                instance: SessionId([0x5a; 16]),
                native_clipboard: false,
            })),
        }),
        true,
    )
    .await;
    assert!(matches!(result, Err(Error::Protocol)));
}

#[tokio::test]
async fn profile_discovery_preserves_a_selected_session_and_v5_operations() {
    for backend in [
        Backend::Iroh,
        #[cfg(feature = "transport-noq")]
        Backend::Noq,
    ] {
        let root = Root::new();
        let local = endpoint(backend).await;
        let remote = endpoint(backend).await;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn({
            let remote = remote.clone();
            async move {
                let connection = remote.accept().await.unwrap().await.unwrap();
                let _ = stopped.await;
                connection.close(0u32.into(), b"capability fixture complete");
            }
        });
        let mut manager = Server::start(
            Some(Prepared::bind(&root.0).await.unwrap()),
            local.clone(),
            None,
        );
        let client = Client::new(&root.0);
        let Reply::Connected(id) = client
            .request(Command::Connect {
                target: rds_net::Ticket::of(&remote).to_string(),
                grant: None,
            })
            .await
            .unwrap()
        else {
            panic!("missing fixture connection");
        };
        let before = client.snapshot().await.unwrap();
        assert_eq!(before.selected, Some(id));
        assert_eq!(before.sessions.len(), 1);
        let capabilities = client.capabilities().await.unwrap().unwrap();
        assert_eq!(capabilities.instance, before.instance);
        assert!(
            !capabilities.native_clipboard,
            "unfinished native proxy must not be advertised"
        );
        // A read-only v6 profile cannot be repurposed to mutate selection.
        let mut stream = socket::connect(&root.0).await.unwrap();
        write_frame(
            &mut stream,
            &Request {
                version: CAPABILITIES_VERSION,
                command: Command::Disconnect { session: id },
            },
        )
        .await
        .unwrap();
        let rejected: Response = read_frame(&mut stream).await.unwrap();
        assert_eq!(rejected.version, CAPABILITIES_VERSION);
        assert_eq!(rejected.result.unwrap_err(), ErrorCode::InvalidRequest);
        drop(stream);
        let after = client.snapshot().await.unwrap();
        assert_eq!(before.instance, after.instance);
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.endpoint, after.endpoint);
        assert_eq!(before.selected, after.selected);
        assert_eq!(after.sessions.len(), 1);
        assert_eq!(after.sessions[0].id, id);
        client
            .request(Command::Disconnect { session: id })
            .await
            .unwrap();
        assert!(client.snapshot().await.unwrap().sessions.is_empty());
        manager.close().await.unwrap();
        local.close().await;
        stop.send(()).unwrap();
        serving.await.unwrap();
        remote.close().await;
    }
}
