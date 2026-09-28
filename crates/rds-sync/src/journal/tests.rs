use super::*;
use crate::fault::{Action, arm};

const OLD: &[u8] = b"prior destination must remain intact";
const NEW: &[u8] = b"complete new verified destination";
const WRITE_POINTS: &[Point] = &[
    Point::Created,
    Point::PartialWrite,
    Point::Written,
    Point::FileSynced,
    Point::Renamed,
    Point::DestinationSynced,
];
const ASSEMBLY_POINTS: &[Point] = &[
    Point::Created,
    Point::PartialWrite,
    Point::Written,
    Point::FileSynced,
    Point::Renamed,
    Point::DestinationSynced,
    Point::SourceSynced,
    Point::PartRemoved,
    Point::MetaRemoved,
    Point::PartsRemoved,
    Point::JournalRemoved,
];

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rds-journal-fault-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("data.bin"), OLD).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(root: &Path, operation: &str, point: Point, action: Action) {
    let manifest = crate::manifest_of(NEW);
    let (armed, result) = match operation {
        "meta" => {
            let armed = arm(point, action);
            let result = Journal::open(root, "data.bin", &manifest).map(drop);
            (armed, result)
        }
        "part" => {
            let mut journal = Journal::open(root, "data.bin", &manifest).unwrap();
            let armed = arm(point, action);
            let result = journal.store(0, NEW).map(drop);
            (armed, result)
        }
        "assembly" => {
            let mut journal = Journal::open(root, "data.bin", &manifest).unwrap();
            journal.store(0, NEW).unwrap();
            let armed = arm(point, action);
            let result = journal.assemble().map(drop);
            (armed, result)
        }
        _ => panic!("unknown test operation"),
    };
    armed.assert_fired();
    let cleanup = matches!(
        point,
        Point::PartRemoved | Point::MetaRemoved | Point::PartsRemoved | Point::JournalRemoved
    );
    assert_eq!(result.is_ok(), cleanup, "{operation}/{point:?}: {result:?}");
}

fn verify_and_resume(root: &Path, operation: &str, point: Point) {
    let committed = operation == "assembly"
        && !matches!(
            point,
            Point::Created | Point::PartialWrite | Point::Written | Point::FileSynced
        );
    assert_eq!(
        std::fs::read(root.join("data.bin")).unwrap(),
        if committed { NEW } else { OLD }
    );
    let manifest = crate::manifest_of(NEW);
    let mut journal = Journal::open(root, "data.bin", &manifest).unwrap();
    let state = root.join(STATE_DIR);
    assert!(!state.join(ASSEMBLY).exists());
    let content = state.join(hex(&manifest.root));
    assert!(!content.join(PENDING).exists());
    assert!(!content.join("parts").join(PENDING).exists());
    if operation == "part" && matches!(point, Point::Renamed | Point::DestinationSynced) {
        assert!(
            journal.complete(),
            "published part must survive process exit"
        );
    }
    for index in journal.need() {
        journal.store(index, NEW).unwrap();
    }
    journal.assemble().unwrap();
    assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), NEW);
    assert!(
        !content.exists(),
        "known completed journal must be collected"
    );
    assert_eq!(
        std::fs::read_dir(&state).unwrap().count(),
        1,
        "only persistent lock remains"
    );
    assert_eq!(
        std::fs::read_dir(root).unwrap().count(),
        2,
        "no staging file may leak beside user data"
    );
    assert!(Journal::open(root, "data.bin", &manifest).is_ok());
}

#[test]
fn io_failures_at_each_commit_and_cleanup_boundary() {
    for errno in [rustix::io::Errno::NOSPC, rustix::io::Errno::ACCESS] {
        for (operation, points) in [
            ("meta", WRITE_POINTS),
            ("part", WRITE_POINTS),
            ("assembly", ASSEMBLY_POINTS),
        ] {
            for &point in points {
                let dir = Scratch::new();
                run(&dir.0, operation, point, Action::Error(errno));
                verify_and_resume(&dir.0, operation, point);
                println!("returned error {errno:?} {operation}/{point:?}: recovered");
            }
        }
    }
}

#[test]
fn crash_child() {
    let Some(root) = std::env::var_os("RDS_JOURNAL_FAULT_ROOT") else {
        return;
    };
    let operation = std::env::var("RDS_JOURNAL_FAULT_OPERATION").unwrap();
    let index: usize = std::env::var("RDS_JOURNAL_FAULT_POINT")
        .unwrap()
        .parse()
        .unwrap();
    let points = if operation == "assembly" {
        ASSEMBLY_POINTS
    } else {
        WRITE_POINTS
    };
    run(Path::new(&root), &operation, points[index], Action::Exit);
    panic!("process-exit fault was not reached");
}

#[test]
fn abrupt_exit_at_each_commit_and_cleanup_boundary() {
    for (operation, points) in [
        ("meta", WRITE_POINTS),
        ("part", WRITE_POINTS),
        ("assembly", ASSEMBLY_POINTS),
    ] {
        for (index, &point) in points.iter().enumerate() {
            let dir = Scratch::new();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "journal::tests::crash_child", "--nocapture"])
                .env("RDS_JOURNAL_FAULT_ROOT", &dir.0)
                .env("RDS_JOURNAL_FAULT_OPERATION", operation)
                .env("RDS_JOURNAL_FAULT_POINT", index.to_string())
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(86),
                "{operation}/{point:?}: {} {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            verify_and_resume(&dir.0, operation, point);
            println!("process exit {operation}/{point:?}: recovered");
        }
    }
}

#[test]
fn recovery_removes_only_reserved_regular_single_link_temporary_files() {
    use std::os::unix::fs::symlink;
    for variant in ["regular", "symlink", "hardlink", "directory"] {
        for name in ["assembly", "pending", "parts/pending"] {
            let dir = Scratch::new();
            let manifest = crate::manifest_of(NEW);
            drop(Journal::open(&dir.0, "data.bin", &manifest).unwrap());
            let state = dir.0.join(STATE_DIR);
            let content = state.join(hex(&manifest.root));
            let temp = if name == ASSEMBLY {
                state.join(name)
            } else {
                content.join(name)
            };
            let sentinel = dir.0.join("sentinel");
            std::fs::write(&sentinel, b"unrelated").unwrap();
            std::fs::write(content.join("unknown"), b"unrelated journal entry").unwrap();
            match variant {
                "regular" => std::fs::write(&temp, b"partial transaction").unwrap(),
                "symlink" => symlink(&sentinel, &temp).unwrap(),
                "hardlink" => std::fs::hard_link(&sentinel, &temp).unwrap(),
                "directory" => std::fs::create_dir(&temp).unwrap(),
                _ => unreachable!(),
            }
            let opened = Journal::open(&dir.0, "data.bin", &manifest);
            assert_eq!(opened.is_ok(), variant == "regular", "{variant}/{name}");
            if let Ok(mut journal) = opened {
                assert!(!temp.exists());
                journal.store(0, NEW).unwrap();
                journal.assemble().unwrap();
            }
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unrelated");
            assert_eq!(
                std::fs::read(content.join("unknown")).unwrap(),
                b"unrelated journal entry"
            );
        }
    }
}

/// Plant a well-formed journal directory without running `Journal::open`,
/// so several dead journals can coexist for the same destination.
fn plant_journal(state: &Path, rel: &str, root: ChunkHash, parts: &[(&ChunkHash, &[u8])]) {
    let dir = state.join(hex(&root));
    std::fs::create_dir_all(dir.join("parts")).unwrap();
    let meta = Meta {
        rel_path: rel.to_string(),
        size: parts.iter().map(|(_, d)| d.len() as u64).sum(),
        root,
    };
    let mut body = postcard::to_allocvec(&meta).unwrap();
    body.extend_from_slice(blake3::hash(&body).as_bytes());
    std::fs::write(dir.join("meta"), &body).unwrap();
    for (hash, data) in parts {
        std::fs::write(dir.join("parts").join(hex(hash)), data).unwrap();
    }
}

#[test]
fn superseded_journals_are_collected_and_foreign_entries_survive() {
    // W1.10: an offer carrying a different root makes the dead journal
    // for the same destination unreachable — a resume can only ever
    // open the offered root. Verified-meta superseded state is removed;
    // foreign, malformed, mislabeled and different-relation entries stay.
    const OTHER: &[u8] = b"other destination remains resumable";
    const NEXT: &[u8] = b"next verified destination content";
    let dir = Scratch::new();
    std::fs::write(dir.0.join("other.bin"), OTHER).unwrap();
    let state = dir.0.join(STATE_DIR);

    let ma = crate::manifest_of(NEW);
    plant_journal(&state, "data.bin", ma.root, &[(&ma.chunks[0].hash, NEW)]);
    let ma2 = crate::manifest_of(b"older superseded content");
    plant_journal(&state, "data.bin", ma2.root, &[]);
    let mo = crate::manifest_of(OTHER);
    plant_journal(&state, "other.bin", mo.root, &[(&mo.chunks[0].hash, OTHER)]);

    let state_a = state.join(hex(&ma.root));
    let state_a2 = state.join(hex(&ma2.root));
    let state_o = state.join(hex(&mo.root));
    std::fs::write(state_a.join("parts").join("foreign.bin"), b"foreign").unwrap();
    // Malformed, mislabeled and foreign siblings must never be swept.
    std::fs::create_dir(state.join("a".repeat(64))).unwrap();
    std::fs::create_dir(state.join("not-a-journal")).unwrap();
    let mislabeled = state.join("b".repeat(64));
    std::fs::create_dir(&mislabeled).unwrap();
    let mut body = postcard::to_allocvec(&Meta {
        rel_path: "data.bin".to_string(),
        size: 0,
        root: ma.root,
    })
    .unwrap();
    body.extend_from_slice(blake3::hash(&body).as_bytes());
    std::fs::write(mislabeled.join("meta"), &body).unwrap();

    let mb = crate::manifest_of(NEXT);
    drop(Journal::open(&dir.0, "data.bin", &mb).unwrap());

    assert!(state.join(hex(&mb.root)).exists(), "own journal must open");
    assert!(!state_a2.exists(), "clean superseded journal collected");
    // Foreign residue keeps the directory; proven-owned names still go.
    assert!(!state_a.join("parts").join(hex(&ma.chunks[0].hash)).exists());
    assert!(state_a.join("parts").join("foreign.bin").exists());
    assert!(
        state_o.join("meta").exists(),
        "other destination stays resumable"
    );
    assert!(state.join("a".repeat(64)).exists());
    assert!(state.join("not-a-journal").exists());
    assert!(mislabeled.join("meta").exists(), "unattributable stays");
    // The surviving different-relation journal still resumes.
    let jo = Journal::open(&dir.0, "other.bin", &mo).unwrap();
    assert!(jo.complete());
}
