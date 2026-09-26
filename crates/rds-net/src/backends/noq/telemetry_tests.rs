//! Actual path churn and delayed consumption of the engine's bounded events.
use super::{Endpoint, bind_endpoint, bind_with_socket, policy, socket, telemetry};
use crate::{Backend, EndpointConfig, PathStatsCoverage};
use noq::{PathStatus, Runtime};
use std::{net::SocketAddr, sync::Arc, time::Duration};

async fn fixture() -> (Endpoint, Endpoint, SocketAddr) {
    let runtime = Arc::new(noq::TokioRuntime);
    let mut sockets = Vec::new();
    for _ in 0..2 {
        sockets.push(
            runtime
                .wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap(),
        );
    }
    let mux = socket::Mux::new(sockets).unwrap();
    let addresses = mux.local_addrs();
    let config = || EndpointConfig {
        backend: Backend::Noq,
        discovery: false,
        bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    };
    // Only the first socket is advertised: the fixture owns every additional
    // open, without QNT reopening the secondary address in the background.
    let server = bind_with_socket(config(), Box::new(mux), vec![addresses[0]], runtime, None)
        .await
        .unwrap();
    (bind_endpoint(config()).await.unwrap(), server, addresses[1])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn high_path_ids_survive_churn_and_lag_is_sticky() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let (client, server, secondary) = fixture().await;
        let (a, b) = tokio::join!(client.connect(server.addr(), rds_core::ALPN), async {
            server.accept().await.unwrap().await
        });
        let (a, b) = (a.unwrap(), b.unwrap());
        let facade = crate::Connection::from(a.clone());
        let initial = facade.path_stats_snapshot();
        assert_eq!(initial.paths.len(), 1, "synchronous handshake seed");
        assert_eq!(initial.paths[0].path_id, 0);
        assert!(initial.paths[0].selected && !initial.paths[0].via_relay);

        // Subscribe now, but deliberately do not poll this observer until the
        // real engine's 32-event broadcast buffer has overflowed.
        let events = a.inner().path_events();
        let qnt = a.inner().nat_traversal_updates();
        let delayed = telemetry::Telemetry::new(a.inner());
        let guard = delayed.guard();
        let mut last = None;
        for iteration in 0..70 {
            let path = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match a
                        .inner()
                        .open_path_ensure(secondary, PathStatus::Backup)
                        .await
                    {
                        Ok(path) => break path,
                        Err(
                            noq::PathError::RemoteCidsExhausted | noq::PathError::MaxPathIdReached,
                        ) => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(error) => panic!("path {iteration}: {error}"),
                    }
                }
            })
            .await
            .unwrap();
            if iteration == 69 {
                last = Some(path);
                break;
            }
            path.close().unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while path.status().is_ok() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        let path = last.unwrap();
        let id = crate::path_id_u64(path.id());
        assert!(id >= 64, "fixture must cross the old scan limit");
        a.inner().path(noq::PathId::ZERO).unwrap().close().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if facade.current_path_stats().is_some_and(|p| p.path_id == id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        a.send_datagram(b"after 70 paths".to_vec().into()).unwrap();
        assert_eq!(&b.read_datagram().await.unwrap()[..], b"after 70 paths");
        assert_eq!(
            facade.path_stats().len(),
            1,
            "closed path history is not live"
        );

        let observer = policy::Observer {
            events,
            telemetry: delayed.clone(),
            transport: None,
        };
        let policy = policy::connection_driver_observed(
            a.inner().weak_handle(),
            qnt,
            observer,
            client.metrics(),
            client.local_addrs().to_vec(),
            Vec::new(),
            None,
            true,
        );
        let task = tokio::spawn(async move {
            let _guard = guard;
            policy.await;
        });
        let lost = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = delayed.snapshot(false);
                if let PathStatsCoverage::PolicyObserved {
                    lost_events,
                    driver_running: true,
                } = snapshot.coverage
                    && lost_events > 0
                    && snapshot.paths.iter().any(|p| p.path_id == id)
                {
                    assert!(snapshot.paths.iter().all(|p| !p.selected));
                    break lost_events;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Repeated publication cannot pretend that missed events were repaired.
        delayed.publish(&delayed.paths(), Some(path.id()));
        assert_eq!(
            delayed.snapshot(false).coverage,
            PathStatsCoverage::PolicyObserved {
                lost_events: lost,
                driver_running: true,
            }
        );
        assert!(delayed.snapshot(false).paths.iter().all(|p| !p.selected));
        let mut observed = a.clone();
        observed.telemetry = delayed.clone();
        let observed = crate::Connection::from(observed);
        assert!(observed.current_path_stats().is_none());
        let registry = crate::metrics::Registry::default();
        let mut sampler = registry.sampler(observed);
        sampler.sample();
        sampler.sample();
        let counters = registry.snapshot();
        assert_eq!(counters["rds_net_policy_observed_connections"], 1);
        assert_eq!(counters["rds_net_degraded_path_observers"], 1);
        assert_eq!(counters["rds_net_path_events_lost_total"], lost);
        let unpolled = telemetry::Telemetry::new(a.inner());
        let guard = unpolled.guard();
        let never_polled = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        drop(never_polled);
        assert!(unpolled.snapshot(false).paths.is_empty());
        assert_eq!(
            unpolled.snapshot(false).coverage,
            PathStatsCoverage::PolicyObserved {
                lost_events: 0,
                driver_running: false,
            }
        );
        assert_eq!(counters["rds_net_selected_path_known"], 0);
        assert_eq!(counters["rds_net_rtt_us"], 0);
        assert_eq!(counters["rds_net_cwnd_bytes"], 0);
        assert!(counters["rds_net_bytes_sent_total{via=\"direct\"}"] > 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(delayed.snapshot(false).paths.is_empty());
        assert_eq!(
            delayed.snapshot(false).coverage,
            PathStatsCoverage::PolicyObserved {
                lost_events: lost,
                driver_running: false,
            }
        );
        sampler.sample();
        assert_eq!(registry.snapshot()["rds_net_live_paths"], 0);
        drop(sampler);
        let counters = registry.snapshot();
        assert_eq!(counters["rds_net_policy_observed_connections"], 0);
        assert_eq!(counters["rds_net_degraded_path_observers"], 0);
        assert_eq!(counters["rds_net_active_connections"], 0);
        assert_eq!(counters["rds_net_path_events_lost_total"], lost);
        client.close().await;
        server.close().await;
        assert!(facade.path_stats().is_empty());
        assert!(facade.current_path_stats().is_none());
    })
    .await
    .expect("path churn or observer shutdown stalled");
}

#[test]
fn path_id_conversion_preserves_boundaries() {
    for value in [0, 63, 64, 1000, u32::MAX] {
        assert_eq!(
            crate::path_id_u64(noq::PathId::from(value)),
            u64::from(value)
        );
    }
}
