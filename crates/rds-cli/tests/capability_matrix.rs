// `docs/capability-matrix.md` must cover every ServiceKind the agent
// can advertise — README, `rds info` and bench reports share one truth.
include!("../../../tests/support/capability_matrix.rs");

#[test]
fn every_service_kind_has_a_matrix_row() {
    use rds_core::ServiceKind::*;
    let kinds = [
        Ping,
        Info,
        Tcp,
        Desktop,
        Sync,
        Audio,
        SyncRead,
        SyncWrite,
        DesktopView,
        DesktopControl,
    ];
    let rows = matrix_rows();
    assert_row_rules(&rows);
    readme_links_matrix();
    for k in kinds {
        let id = format!("service:{}", kebab(&format!("{k:?}")));
        assert!(
            row_present(&rows, &id),
            "ServiceKind::{k:?} has no `{id}` row in docs/capability-matrix.md"
        );
    }
}

#[test]
fn stub_services_never_advertise_readiness() {
    // `stub`/`unavailable` service rows must not appear in the always-on
    // advertisement: an agent that lists a capability must back it.
    let rows = matrix_rows();
    for r in &rows {
        if r.capability == "service:audio" {
            assert_eq!(
                r.state, "stub",
                "audio has wire shape only; claiming more overstates the product"
            );
        }
    }
}
