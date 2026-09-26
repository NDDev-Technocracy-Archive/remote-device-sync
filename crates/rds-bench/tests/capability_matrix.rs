// Every lane `rds-bench` can emit into a report must be a declared row
// in `docs/capability-matrix.md` — a report name that resolves to no
// declared capability is evidence nobody can interpret.
include!("../../../tests/support/capability_matrix.rs");

#[test]
fn every_bench_lane_has_a_matrix_row() {
    let rows = matrix_rows();
    assert_row_rules(&rows);
    readme_links_matrix();
    for lane in rds_bench::scenario::LANES {
        let id = format!("measure:{}", lane.name());
        assert!(
            row_present(&rows, &id),
            "bench lane {:?} has no `{id}` row in docs/capability-matrix.md",
            lane.name()
        );
    }
}
