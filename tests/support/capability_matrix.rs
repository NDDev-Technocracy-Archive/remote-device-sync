// Shared checker for `docs/capability-matrix.md` (W0.5). Included by
// crate tests that own an enum the matrix must cover, so the doc stays
// honest instead of drifting from the code.
// Including targets run `matrix_rows()`, `assert_row_rules()` and
// `readme_links_matrix()` plus their own coverage assertions.

use std::path::PathBuf;

const STATES: [&str; 4] = ["implemented", "experimental", "stub", "unavailable"];

fn root() -> PathBuf {
    // crates/<krate>/tests/*.rs -> workspace root is ../.. from manifest dir.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One row: capability id, state, prerequisites, evidence.
struct Row {
    capability: String,
    state: String,
    prereqs: String,
    evidence: String,
}

fn matrix_rows() -> Vec<Row> {
    let text = std::fs::read_to_string(root().join("docs/capability-matrix.md"))
        .expect("docs/capability-matrix.md must exist");
    assert!(
        text.contains("matrix_version:"),
        "matrix must carry a matrix_version header"
    );
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') || line.contains("---") || line.contains("Capability") {
            continue;
        }
        let cells: Vec<&str> = line
            .split('|')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        assert_eq!(
            cells.len(),
            4,
            "matrix row must have 4 cells: capability/state/prereqs/evidence — {line:?}"
        );
        rows.push(Row {
            capability: cells[0].to_string(),
            state: cells[1].to_string(),
            prereqs: cells[2].to_string(),
            evidence: cells[3].to_string(),
        });
    }
    assert!(!rows.is_empty(), "matrix parsed zero rows");
    rows
}

/// State vocabulary is closed; placeholders must name what is missing,
/// real rows must name their evidence.
fn assert_row_rules(rows: &[Row]) {
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        assert!(
            STATES.contains(&r.state.as_str()),
            "row {:?} has unknown state {:?} (allowed: {STATES:?})",
            r.capability,
            r.state
        );
        assert!(
            seen.insert(&r.capability),
            "duplicate matrix row {:?}",
            r.capability
        );
        match r.state.as_str() {
            "stub" | "unavailable" => assert!(
                r.prereqs != "—" && !r.prereqs.is_empty(),
                "placeholder row {:?} must name what would make it real",
                r.capability
            ),
            _ => assert!(
                !r.evidence.is_empty() && r.evidence != "—",
                "real row {:?} must name evidence",
                r.capability
            ),
        }
    }
}

/// README is the public face; it must point at the matrix.
fn readme_links_matrix() {
    let readme = std::fs::read_to_string(root().join("README.md")).expect("README.md");
    assert!(
        readme.contains("docs/capability-matrix.md"),
        "README.md must link docs/capability-matrix.md"
    );
}

fn row_present(rows: &[Row], capability: &str) -> bool {
    rows.iter().any(|r| r.capability == capability)
}

/// CamelCase enum variant -> kebab-case matrix id segment.
/// Used only by includers that map enum variants onto `service:*` ids.
#[allow(dead_code)]
fn kebab(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
