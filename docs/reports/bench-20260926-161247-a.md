# rds bench suite — 1790439552

commit: `f6c780b`

## handshake (iroh, direct)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 21.28ms | 25.12ms | 34.22ms | 35.11ms | 39.32ms | 26.42ms |

attempts: 100/100 succeeded

metrics:
- `agent_rds_net_connections_accepted_total` = 99
- `client_rds_net_bytes_received_total{via="direct"}` = 133800
- `client_rds_net_bytes_sent_total{via="direct"}` = 495185
- `client_rds_net_connections_opened_total` = 100
- `client_rds_net_datagrams_sent_total{via="direct"}` = 585
- `client_rds_net_paths_seen_total{via="direct"}` = 100


## ping (iroh, direct)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 1.52ms | 3.47ms | 10.31ms | 20.26ms | 21.27ms | 4.51ms |

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="direct"}` = 7051
- `agent_rds_net_bytes_sent_total{via="direct"}` = 6205
- `agent_rds_net_connections_accepted_total` = 1
- `agent_rds_net_cwnd_bytes` = 15009
- `agent_rds_net_datagrams_sent_total{via="direct"}` = 22
- `agent_rds_net_live_paths` = 1
- `agent_rds_net_paths_seen_total{via="direct"}` = 1
- `agent_rds_net_rtt_us` = 16520
- `agent_rds_net_selected_path_known` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="direct"}` = 48302
- `client_rds_net_bytes_sent_total{via="direct"}` = 37288
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 5808
- `client_rds_net_datagrams_sent_total{via="direct"}` = 282
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="direct"}` = 1
- `client_rds_net_rtt_us` = 795
- `client_rds_net_selected_path_known` = 1


## transfer (iroh, direct)

throughput: **11.8 MiB/s**

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="direct"}` = 22607138
- `agent_rds_net_bytes_sent_total{via="direct"}` = 59627
- `agent_rds_net_congestion_events_total` = 7
- `agent_rds_net_connections_accepted_total` = 1
- `agent_rds_net_cwnd_bytes` = 5808
- `agent_rds_net_datagrams_lost_total{via="direct"}` = 45
- `agent_rds_net_datagrams_sent_total{via="direct"}` = 524
- `agent_rds_net_live_paths` = 1
- `agent_rds_net_paths_seen_total{via="direct"}` = 1
- `agent_rds_net_rtt_us` = 1390
- `agent_rds_net_selected_path_known` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="direct"}` = 58941
- `client_rds_net_bytes_sent_total{via="direct"}` = 30499958
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 738461
- `client_rds_net_datagrams_sent_total{via="direct"}` = 21050
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="direct"}` = 1
- `client_rds_net_rtt_us` = 1773
- `client_rds_net_selected_path_known` = 1


## multiconnect (iroh, direct-impaired)

impairment: `loss=5.0% delay=50ms jitter=30ms rate=uncapped seed=1`

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 131.48ms | 160.64ms | 1156.31ms | 1286.81ms | 1310.70ms | 283.46ms |

attempts: 100/100 succeeded

metrics:
- `agent_rds_net_bytes_received_total{via="direct"}` = 53219
- `agent_rds_net_bytes_sent_total{via="direct"}` = 76864
- `agent_rds_net_congestion_events_total` = 2
- `agent_rds_net_connections_accepted_total` = 61
- `agent_rds_net_datagrams_lost_total{via="direct"}` = 2
- `agent_rds_net_datagrams_sent_total{via="direct"}` = 114
- `agent_rds_net_paths_seen_total{via="direct"}` = 17
- `bench_impair_dropped_datagrams` = 62
- `bench_impair_forwarded_bytes` = 710370
- `bench_impair_forwarded_datagrams` = 1101
- `bench_impair_probes` = 1
- `client_rds_net_bytes_received_total{via="direct"}` = 142007
- `client_rds_net_bytes_sent_total{via="direct"}` = 530552
- `client_rds_net_congestion_events_total` = 11
- `client_rds_net_connections_opened_total` = 100
- `client_rds_net_datagrams_lost_total{via="direct"}` = 11
- `client_rds_net_datagrams_sent_total{via="direct"}` = 617
- `client_rds_net_paths_seen_total{via="direct"}` = 100

- impairment probes=1 forwarded=1101 dropped=62 bytes=710370

## relay-fallback (iroh, relay)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 3.40ms | 6.37ms | 12.26ms | 16.22ms | 16.70ms | 7.02ms |

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="relay"}` = 5265
- `agent_rds_net_bytes_sent_total{via="relay"}` = 5605
- `agent_rds_net_connections_accepted_total` = 1
- `agent_rds_net_cwnd_bytes` = 15006
- `agent_rds_net_datagrams_sent_total{via="relay"}` = 10
- `agent_rds_net_live_paths` = 1
- `agent_rds_net_paths_seen_total{via="relay"}` = 1
- `agent_rds_net_rtt_us` = 19730
- `agent_rds_net_selected_path_known` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="relay"}` = 15469
- `client_rds_net_bytes_sent_total{via="relay"}` = 15874
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 9995
- `client_rds_net_datagrams_sent_total{via="relay"}` = 182
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="relay"}` = 1
- `client_rds_net_rtt_us` = 4214
- `client_rds_net_selected_path_known` = 1


## impaired (iroh, direct-impaired)

impairment: `loss=5.0% delay=50ms jitter=30ms rate=uncapped seed=1`

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 111.41ms | 140.71ms | 460.64ms | 575.03ms | 587.17ms | 175.28ms |

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="direct"}` = 97684
- `agent_rds_net_bytes_sent_total{via="direct"}` = 133209
- `agent_rds_net_congestion_events_total` = 25
- `agent_rds_net_connections_accepted_total` = 1
- `agent_rds_net_cwnd_bytes` = 6133
- `agent_rds_net_datagrams_lost_total{via="direct"}` = 136
- `agent_rds_net_datagrams_sent_total{via="direct"}` = 653
- `agent_rds_net_live_paths` = 1
- `agent_rds_net_paths_seen_total{via="direct"}` = 6
- `agent_rds_net_rtt_us` = 160262
- `agent_rds_net_selected_path_known` = 1
- `bench_impair_dropped_datagrams` = 58
- `bench_impair_forwarded_bytes` = 89140
- `bench_impair_forwarded_datagrams` = 956
- `bench_impair_probes` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="direct"}` = 105537
- `client_rds_net_bytes_sent_total{via="direct"}` = 83144
- `client_rds_net_congestion_events_total` = 31
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 5901
- `client_rds_net_datagrams_lost_total{via="direct"}` = 131
- `client_rds_net_datagrams_sent_total{via="direct"}` = 618
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="direct"}` = 1
- `client_rds_net_rtt_us` = 134934
- `client_rds_net_selected_path_known` = 1

- impairment probes=1 forwarded=961 dropped=58 bytes=89326

## impaired (iroh, relay-impaired)

impairment: `loss=5.0% delay=50ms jitter=30ms rate=uncapped seed=1`

- SCENARIO SKIPPED: built without transport-noq; the owned relay backend is unavailable

## resolve-connect (iroh, discovered)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 100 | 86.44ms | 127.92ms | 173.46ms | 288.46ms | 289.69ms | 134.30ms |

attempts: 100/100 succeeded

metrics:
- `agent_rds_net_bytes_received_total{via="direct"}` = 859229
- `agent_rds_net_bytes_sent_total{via="direct"}` = 764903
- `agent_rds_net_connections_accepted_total` = 100
- `agent_rds_net_datagrams_sent_total{via="direct"}` = 2436
- `agent_rds_net_paths_seen_total{via="direct"}` = 100
- `client_rds_net_bytes_received_total{via="direct"}` = 1062373
- `client_rds_net_bytes_sent_total{via="direct"}` = 1399868
- `client_rds_net_connections_opened_total` = 100
- `client_rds_net_datagrams_sent_total{via="direct"}` = 2609
- `client_rds_net_paths_seen_total{via="direct"}` = 111


