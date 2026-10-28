//! Rank confirmed paths by RTT, optionally ordering configured relay reserves.
use std::time::Duration;

use iroh::endpoint::transports::{PathSelection, PathSelectionContext, PathSelector};

const SWITCH_GAIN: Duration = Duration::from_millis(5);

#[derive(Debug)]
pub(super) struct LatencySelector;

impl PathSelector for LatencySelector {
    fn maintain_standby_paths(&self) -> bool {
        true
    }
    fn refresh_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }

    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        select(ctx, None)
    }
}

#[derive(Debug)]
pub(super) struct OrderedRelaySelector {
    pub(super) order: Vec<iroh::RelayUrl>,
}

impl PathSelector for OrderedRelaySelector {
    fn maintain_standby_paths(&self) -> bool {
        true
    }
    fn refresh_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        select(ctx, Some(&self.order))
    }
}

fn relay_rank(
    path: &iroh::endpoint::transports::FourTuple,
    order: &[iroh::RelayUrl],
) -> Option<usize> {
    match path {
        iroh::endpoint::transports::FourTuple::Relay { url, .. } => Some(
            order
                .iter()
                .position(|item| item == url)
                .unwrap_or(usize::MAX),
        ),
        _ => None,
    }
}

fn select(ctx: &PathSelectionContext<'_>, order: Option<&[iroh::RelayUrl]>) -> PathSelection {
    let mut paths: Vec<_> = ctx
            .paths()
            .filter_map(|path| {
                let stats = path.stats()?;
                let rtt = stats.rtt;
                let progress = path
                    .congestion_state()
                    .and_then(crate::ack_progress::snapshot);
                if progress.is_some_and(|state| state.needs_probe()) {
                    path.ping();
                }
                tracing::debug!(target: "rds_net::path_policy",
                    selected=Some(path.network_path()) == ctx.current(),
                    stream_work=stats.unacknowledged_stream_frames,
                    path_rtt_ms=rtt.as_millis(),
                    pending_ack_age_ms=?progress.and_then(|s|s.pending_age()).map(|d|d.as_millis()),
                    confirmed_ack_age_ms=?progress.and_then(|s|s.confirmation_age()).map(|d|d.as_millis()),
                    "latency path progress observation");
                Some((path, rtt, progress, stats.unacknowledged_stream_frames))
            })
            .collect();
    if let Some(order) = order {
        paths.sort_by_key(|(path, rtt, _, _)| {
            (
                relay_rank(path.network_path(), order).map_or(0, |rank| rank.saturating_add(1)),
                *rtt,
            )
        });
    }
    for (failed, rtt, progress, stream_work) in &paths {
        if !stream_work {
            continue;
        }
        if !progress.is_some_and(|state| state.unresponsive(*rtt)) {
            continue;
        }
        for (fallback, other_rtt, state, _) in &paths {
            if (*progress)
                .zip(*state)
                .is_some_and(|(failed, sibling)| sibling.can_replace(failed, *other_rtt))
                && failed.abandon_with_fallback(fallback)
            {
                tracing::warn!(target:"rds_net::path_policy",
                        pending_ack_age_ms=?progress.and_then(|state| state.pending_age()).map(|age| age.as_millis()),
                        fallback_ack_age_ms=?state.and_then(|state| state.confirmation_age()).map(|age| age.as_millis()),
                        path_rtt_ms=rtt.as_millis(), fallback_rtt_ms=other_rtt.as_millis(),
                        "unresponsive path retired with a confirmed sibling; reliable streams retained");
                break;
            }
        }
    }
    let has_confirmed = paths.iter().any(|(_, rtt, state, _)| {
        state.is_some_and(|state| state.confirmed(*rtt) && !state.stalled(*rtt))
    });
    let eligible = paths
        .into_iter()
        .filter_map(|(path, rtt, state, _)| {
            if state.is_some_and(|state| state.stalled(rtt))
                || (has_confirmed && state.is_some_and(|state| !state.confirmed(rtt)))
            {
                return None;
            }
            let current = Some(path.network_path()) == ctx.current();
            let rank = order.and_then(|order| relay_rank(path.network_path(), order));
            Some((path, rtt, current, rank))
        })
        .collect();
    let choice = choose_with_relay_order(eligible);
    let mut selection = PathSelection::none();
    if let Some(path) = choice {
        selection.set(&path);
    }
    selection
}

fn choose_with_relay_order<T>(paths: Vec<(T, Duration, bool, Option<usize>)>) -> Option<T> {
    let best_relay = paths.iter().filter_map(|(_, _, _, rank)| *rank).min();
    choose(paths.into_iter().filter_map(|(path, rtt, current, rank)| {
        (rank.is_none() || rank == best_relay).then_some((path, rtt, current))
    }))
}

fn choose<T>(paths: impl Iterator<Item = (T, Duration, bool)>) -> Option<T> {
    let mut best: Option<(T, Duration)> = None;
    let mut current: Option<Duration> = None;
    for (path, rtt, selected) in paths {
        if selected && current.is_none_or(|old| rtt < old) {
            current = Some(rtt);
        }
        if best.as_ref().is_none_or(|(_, old)| rtt < *old) {
            best = Some((path, rtt));
        }
    }
    let (path, rtt) = best?;
    if current.is_none_or(|old| old.saturating_sub(rtt) >= SWITCH_GAIN) {
        Some(path)
    } else {
        // The Iroh selector contract interprets an empty selection as keep current.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Clone, Copy)]
    enum Route {
        Direct,
        Relay,
    }

    #[test]
    fn relay_order_retains_fast_direct_and_uses_first_healthy_reserve() {
        let ms = Duration::from_millis;
        assert_eq!(
            choose_with_relay_order(vec![
                ("direct", ms(70), true, None),
                ("primary", ms(100), false, Some(0)),
                ("secondary", ms(20), false, Some(1))
            ]),
            None
        );
        assert_eq!(
            choose_with_relay_order(vec![
                ("primary", ms(100), false, Some(0)),
                ("secondary", ms(20), true, Some(1)),
                ("last", ms(1), false, Some(2))
            ]),
            Some("primary")
        );
        assert_eq!(
            choose_with_relay_order(vec![
                ("secondary", ms(100), false, Some(1)),
                ("last", ms(1), true, Some(2))
            ]),
            Some("secondary")
        );
        assert_eq!(
            choose_with_relay_order(vec![
                ("direct", ms(20), false, None),
                ("primary", ms(70), true, Some(0))
            ]),
            Some("direct")
        );
    }

    #[test]
    fn usable_direct_path_does_not_hide_a_faster_relay() {
        let paths = [
            (Route::Direct, Duration::from_millis(170), true),
            (Route::Relay, Duration::from_millis(65), false),
        ];
        assert_eq!(choose(paths.into_iter()), Some(Route::Relay));
        assert_eq!(choose(paths.into_iter().rev()), Some(Route::Relay));
    }

    #[test]
    fn stickiness_ties_missing_current_and_extreme_values_are_explicit() {
        let ms = Duration::from_millis;
        assert_eq!(
            choose([(Route::Direct, ms(69), true), (Route::Relay, ms(65), false)].into_iter()),
            None
        );
        assert_eq!(
            choose([(Route::Direct, ms(70), true), (Route::Relay, ms(65), false)].into_iter()),
            Some(Route::Relay)
        );
        assert_eq!(
            choose([(Route::Direct, ms(65), true), (Route::Relay, ms(65), false)].into_iter()),
            None
        );
        assert_eq!(
            choose([(Route::Relay, ms(65), false)].into_iter()),
            Some(Route::Relay)
        );
        assert_eq!(
            choose(
                [
                    (Route::Direct, Duration::MAX, true),
                    (Route::Relay, Duration::MAX, false)
                ]
                .into_iter()
            ),
            None
        );
        assert_eq!(choose(std::iter::empty::<(Route, Duration, bool)>()), None);
    }
}
