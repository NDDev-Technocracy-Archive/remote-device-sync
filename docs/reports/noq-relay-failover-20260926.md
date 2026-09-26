# rds bench suite — 1790462409

commit: `94e4ecf`

## migration (noq, relay-failover-drain)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 5 | 7.07ms | 10.41ms | 27.33ms | 27.33ms | 27.33ms | 13.27ms |

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="relay"}` = 10877
- `agent_rds_net_bytes_sent_total{via="relay"}` = 26687
- `agent_rds_net_connections_accepted_total` = 2
- `agent_rds_net_cwnd_bytes` = 16318
- `agent_rds_net_datagrams_sent_total{via="relay"}` = 68
- `agent_rds_net_live_paths` = 2
- `agent_rds_net_paths_seen_total{via="relay"}` = 2
- `agent_rds_net_policy_observed_connections` = 1
- `agent_rds_net_rtt_us` = 5141
- `agent_rds_net_selected_path_known` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="relay"}` = 5433
- `client_rds_net_bytes_sent_total{via="relay"}` = 14767
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 17884
- `client_rds_net_datagrams_sent_total{via="relay"}` = 45
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="relay"}` = 1
- `client_rds_net_policy_observed_connections` = 1
- `client_rds_net_rtt_us` = 8942
- `client_rds_net_selected_path_known` = 1
- `migration_recovery_ns` = 9527321
- `migration_slot_after` = 1
- `relay0_forwarded_bytes` = 11357
- `relay0_forwarded_datagrams` = 52
- `relay1_forwarded_bytes` = 17381
- `relay1_forwarded_datagrams` = 90

- relay-failover-drain: slot 0 failed → resumed 9.527321ms on slot 1, 0 probes lost in flight; post-migration RTT from the surviving slot

## migration (noq, relay-failover-kill)

| n | min | p50 | p95 | p99 | max | mean |
|---|-----|-----|-----|-----|-----|------|
| 5 | 7.46ms | 9.04ms | 12.41ms | 12.41ms | 12.41ms | 9.39ms |

metrics:
- `agent_rds_net_active_connections` = 1
- `agent_rds_net_bytes_received_total{via="relay"}` = 2887
- `agent_rds_net_bytes_sent_total{via="relay"}` = 3285
- `agent_rds_net_connections_accepted_total` = 2
- `agent_rds_net_cwnd_bytes` = 12143
- `agent_rds_net_datagrams_sent_total{via="relay"}` = 8
- `agent_rds_net_live_paths` = 1
- `agent_rds_net_paths_seen_total{via="relay"}` = 1
- `agent_rds_net_policy_observed_connections` = 1
- `agent_rds_net_rtt_us` = 26139
- `agent_rds_net_selected_path_known` = 1
- `client_rds_net_active_connections` = 1
- `client_rds_net_bytes_received_total{via="relay"}` = 5637
- `client_rds_net_bytes_sent_total{via="relay"}` = 14806
- `client_rds_net_connections_opened_total` = 1
- `client_rds_net_cwnd_bytes` = 16960
- `client_rds_net_datagrams_sent_total{via="relay"}` = 38
- `client_rds_net_live_paths` = 1
- `client_rds_net_paths_seen_total{via="relay"}` = 1
- `client_rds_net_policy_observed_connections` = 1
- `client_rds_net_rtt_us` = 8695
- `client_rds_net_selected_path_known` = 1
- `migration_recovery_ns` = 121637177
- `migration_slot_before` = 1
- `relay0_forwarded_bytes` = 12112
- `relay0_forwarded_datagrams` = 63
- `relay1_forwarded_bytes` = 15463
- `relay1_forwarded_datagrams` = 41

- relay-failover-kill: slot 1 failed → resumed 121.637177ms on slot 0, 0 probes lost in flight; post-migration RTT from the surviving slot

