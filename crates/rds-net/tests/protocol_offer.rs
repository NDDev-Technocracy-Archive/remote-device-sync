//! A caller may admit explicit alternatives without guessing from a failed dial.
use rds_net::{Backend, Connection, Endpoint, EndpointConfig, bind_endpoint};
use std::time::Duration;

const FIRST: &[u8] = b"rds-test-first/0";
const SECOND: &[u8] = b"rds-test-second/0";

fn backends() -> Vec<Backend> {
    #[cfg(feature = "transport-noq")]
    {
        vec![Backend::Iroh, Backend::Noq]
    }
    #[cfg(not(feature = "transport-noq"))]
    {
        vec![Backend::Iroh]
    }
}

fn protocols(values: &[&[u8]]) -> Vec<Vec<u8>> {
    values.iter().map(|value| value.to_vec()).collect()
}

async fn endpoint(backend: Backend, alpns: &[&[u8]]) -> Endpoint {
    bind_endpoint(EndpointConfig {
        backend,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        alpns: protocols(alpns),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn pair(client: &Endpoint, server: &Endpoint, offer: &[&[u8]]) -> (Connection, Connection) {
    let offer = protocols(offer);
    let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(client.connect_offered(server.addr(), &offer), async {
            server.accept().await.unwrap().await
        })
    })
    .await
    .unwrap();
    let outgoing = outgoing.unwrap();
    let incoming = incoming.unwrap();
    assert_eq!(outgoing.remote_id(), server.id());
    assert_eq!(incoming.remote_id(), client.id());
    (outgoing, incoming)
}

async fn assert_protocol(pair: (Connection, Connection), expected: &[u8]) {
    let (outgoing, incoming) = pair;
    assert_eq!(outgoing.alpn().as_deref(), Some(expected));
    assert_eq!(incoming.alpn().as_deref(), Some(expected));
    outgoing.send_datagram(expected.to_vec().into()).unwrap();
    let body = tokio::time::timeout(Duration::from_secs(2), incoming.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&body[..], expected);
    outgoing.close(0u32.into(), b"offer qualification complete");
    incoming.close(0u32.into(), b"offer qualification complete");
}

#[tokio::test]
async fn explicit_offers_use_actual_peer_preference_across_backends() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[SECOND, FIRST]).await;
            let server = endpoint(server_backend, &[FIRST, SECOND]).await;
            // Repeated offers on the same endpoints must not reuse an earlier
            // protocol selection or substitute the client's first identifier.
            for (offer, expected) in [
                (&[SECOND, FIRST][..], FIRST),
                (&[SECOND][..], SECOND),
                (&[SECOND, FIRST][..], FIRST),
            ] {
                assert_protocol(pair(&client, &server, offer).await, expected).await;
            }
            client.close().await;
            server.close().await;
        }
    }
}

#[tokio::test]
async fn an_explicit_legacy_alternative_works_with_a_legacy_protocol_peer() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[SECOND, FIRST]).await;
            let legacy = endpoint(server_backend, &[FIRST]).await;
            assert_protocol(pair(&client, &legacy, &[SECOND, FIRST]).await, FIRST).await;
            client.close().await;
            legacy.close().await;
        }
    }
}

#[tokio::test]
async fn an_exact_legacy_request_works_when_the_peer_prefers_a_new_protocol() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[FIRST]).await;
            let server = endpoint(server_backend, &[SECOND, FIRST]).await;
            let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(client.connect(server.addr(), FIRST), async {
                    server.accept().await.unwrap().await
                })
            })
            .await
            .unwrap();
            assert_protocol((outgoing.unwrap(), incoming.unwrap()), FIRST).await;
            client.close().await;
            server.close().await;
        }
    }
}

#[tokio::test]
async fn missing_common_protocol_refuses_without_an_implicit_alternative() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[SECOND, FIRST]).await;
            let server = endpoint(server_backend, &[FIRST]).await;
            let offer = protocols(&[SECOND]);
            let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(client.connect_offered(server.addr(), &offer), async {
                    server.accept().await.unwrap().await
                })
            })
            .await
            .unwrap();
            client.close().await;
            server.close().await;
            assert!(
                outgoing.is_err() && incoming.is_err(),
                "an unoffered protocol connected"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_exact_and_alternative_offers_cannot_mutate_each_other() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[FIRST, SECOND]).await;
            let server = endpoint(server_backend, &[FIRST, SECOND]).await;
            let offer = protocols(&[SECOND, FIRST]);
            let (offered, exact, incoming) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(
                    client.connect_offered(server.addr(), &offer),
                    client.connect(server.addr(), SECOND),
                    async {
                        let first = server.accept().await.unwrap();
                        let second = server.accept().await.unwrap();
                        let (a, b) = tokio::join!(first, second);
                        (a.unwrap(), b.unwrap())
                    }
                )
            })
            .await
            .unwrap();
            let offered = offered.unwrap();
            let exact = exact.unwrap();
            assert_eq!(offered.alpn().as_deref(), Some(FIRST));
            assert_eq!(exact.alpn().as_deref(), Some(SECOND));
            let (first, second) = incoming;
            let (a, b) = if first.alpn().as_deref() == Some(FIRST) {
                (first, second)
            } else {
                (second, first)
            };
            assert_protocol((offered, a), FIRST).await;
            assert_protocol((exact, b), SECOND).await;
            client.close().await;
            server.close().await;
        }
    }
}

#[tokio::test]
async fn malformed_or_locally_unconfigured_offers_fail_before_dialing() {
    for backend in backends() {
        let client = endpoint(backend, &[FIRST]).await;
        let target = rds_net::EndpointAddr {
            id: rds_net::SecretKey::generate().public(),
            addrs: Default::default(),
        };
        let malformed = [
            vec![],
            vec![vec![]],
            vec![vec![0; 256]],
            protocols(&[FIRST, FIRST]),
            (0..17).map(|i| vec![i]).collect(),
        ];
        for offer in malformed {
            let error = client
                .connect_offered(target.clone(), &offer)
                .await
                .unwrap_err();
            assert!(
                error.downcast_ref::<rds_net::ConfigError>().is_some(),
                "invalid offer reached address resolution: {error}"
            );
        }
        let error = client
            .connect_offered(target, &protocols(&[SECOND]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not configured"));
        client.close().await;
    }
}

#[tokio::test]
async fn protocol_alternatives_do_not_relax_the_expected_peer_key() {
    for client_backend in backends() {
        for server_backend in backends() {
            let client = endpoint(client_backend, &[FIRST, SECOND]).await;
            let server = endpoint(server_backend, &[FIRST, SECOND]).await;
            let mut target = server.addr();
            target.id = rds_net::SecretKey::generate().public();
            let offer = protocols(&[SECOND, FIRST]);
            let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(client.connect_offered(target, &offer), async {
                    server.accept().await.unwrap().await
                })
            })
            .await
            .unwrap();
            if let Ok(connection) = incoming {
                connection.close(0u32.into(), b"refused peer test complete");
            }
            client.close().await;
            server.close().await;
            assert!(
                outgoing.is_err(),
                "protocol negotiation changed the authenticated peer"
            );
        }
    }
}
