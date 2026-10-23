use super::*;
use ed25519_dalek::SigningKey;
use std::{path::PathBuf, time::Duration};

pub(super) const PHASES: [Phase; 3] = [
    Phase::BeforeDatabase,
    Phase::AfterDatabase,
    Phase::AfterAnchor,
];
pub(super) struct Temp(pub(super) PathBuf);
impl Temp {
    pub(super) fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rds-record-crash-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub(super) fn signer() -> SigningKey {
    SigningKey::from_bytes(&[82; 32])
}
pub(super) fn record() -> EndpointRecord {
    EndpointRecord::publish(
        &signer(),
        1,
        vec!["127.0.0.1:4000".parse().unwrap()],
        vec![],
        vec![crate::Service::Ping],
        Duration::from_secs(300),
    )
    .unwrap()
}
pub(super) fn key() -> EndpointKey {
    EndpointKey(signer().verifying_key().to_bytes())
}

#[test]
fn failed_commit_never_publishes_and_reopen_reconciles_one_unacknowledged_generation() {
    for phase in PHASES {
        let tmp = Temp::new();
        let store = FileStore::new(&tmp.0).unwrap();
        store.put(&record()).unwrap();
        let metrics = store.metrics().unwrap();
        assert_eq!(metrics.snapshot().unwrap().generation, Some(2));
        store.inner.lock().unwrap().fault = Some((phase, false));
        assert!(
            store
                .remove(&DeleteRequest::new(&signer(), 2).unwrap())
                .is_err()
        );
        assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
        let failed = metrics.snapshot().unwrap();
        assert!(!failed.healthy);
        assert_eq!(failed.generation, Some(2));
        assert_eq!(failed.records, 1);
        drop(store);
        assert!(metrics.snapshot().is_none());
        let reopened = FileStore::new(&tmp.0).unwrap();
        let recovered = reopened.metrics().unwrap().snapshot().unwrap();
        assert!(recovered.healthy && recovered.durable);
        assert_eq!(
            recovered.generation,
            Some(if phase == Phase::BeforeDatabase { 2 } else { 3 })
        );
        assert_eq!(recovered.records, u64::from(phase == Phase::BeforeDatabase));
        assert_eq!(recovered.identities, 1);
        assert_eq!(
            reopened.get(&key()).is_ok(),
            phase == Phase::BeforeDatabase,
            "{phase:?}"
        );
    }
}

#[test]
fn every_anchor_failure_keeps_the_database_commit_unpublished_until_recovery() {
    use crate::persist::Phase;
    for phase in [
        Phase::BeforeWrite,
        Phase::AfterWrite,
        Phase::AfterFileSync,
        Phase::AfterRename,
        Phase::AfterDirectorySync,
    ] {
        let tmp = Temp::new();
        let store = FileStore::new(&tmp.0).unwrap();
        store.put(&record()).unwrap();
        store.inner.lock().unwrap().anchor.fault = Some((phase, false));
        assert!(
            store
                .remove(&DeleteRequest::new(&signer(), 2).unwrap())
                .is_err()
        );
        assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
        drop(store);
        let reopened = FileStore::new(&tmp.0).unwrap();
        assert!(
            matches!(reopened.get(&key()), Err(DiscoveryError::NotFound)),
            "{phase:?}"
        );
    }
}

#[test]
fn partial_database_write_and_sync_failure_do_not_reset_committed_history() {
    for mode in [1, 2] {
        let tmp = Temp::new();
        let store = FileStore::new(&tmp.0).unwrap();
        let record = record();
        store.put(&record).unwrap();
        let fault = store.inner.lock().unwrap().io_fault.clone();
        fault.store(mode, std::sync::atomic::Ordering::SeqCst);
        assert!(
            store
                .remove(&DeleteRequest::new(&signer(), 2).unwrap())
                .is_err()
        );
        assert_eq!(fault.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
        drop(store);
        let reopened = FileStore::new(&tmp.0).unwrap();
        match reopened.get(&key()) {
            Ok(stored) => assert_eq!(stored.payload, record.payload),
            Err(DiscoveryError::NotFound) => {
                assert!(matches!(reopened.put(&record), Err(DiscoveryError::Stale)))
            }
            Err(error) => panic!("unexpected recovered state: {error}"),
        }
    }
}

#[test]
fn backend_bounds_reject_overflow_and_growth_before_writing() {
    let tmp = Temp::new();
    let file = File::create(tmp.0.join("bounded")).unwrap();
    let backend = BoundedFile {
        file,
        fault: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };
    use redb::StorageBackend;
    assert!(backend.set_len(MAX_DATABASE + 1).is_err());
    assert!(backend.write(MAX_DATABASE - 1, &[1, 2]).is_err());
    assert!(backend.write(u64::MAX, &[1]).is_err());
    assert_eq!(backend.len().unwrap(), 0);
}

#[test]
fn backend_drop_releases_its_lock_despite_an_inherited_descriptor_alias() {
    let tmp = Temp::new();
    let path = tmp.0.join("locked");
    let file = File::create(&path).unwrap();
    file.try_lock().unwrap();
    let inherited = file.try_clone().unwrap();
    let backend = BoundedFile {
        file,
        fault: Default::default(),
    };
    let successor = File::options().read(true).write(true).open(path).unwrap();
    assert!(successor.try_lock().is_err());
    drop(backend);
    successor.try_lock().unwrap();
    drop(inherited);
}

#[test]
fn crash_writer() {
    let Some(path) = std::env::var_os("RDS_TEST_RECORD_CRASH_DIRECTORY") else {
        return;
    };
    let index: usize = std::env::var("RDS_TEST_RECORD_CRASH_PHASE")
        .unwrap()
        .parse()
        .unwrap();
    let store = FileStore::new(PathBuf::from(path)).unwrap();
    store.inner.lock().unwrap().fault = Some((PHASES[index], true));
    store
        .remove(&DeleteRequest::new(&signer(), 2).unwrap())
        .unwrap();
    panic!("crash checkpoint did not stop child");
}

#[test]
fn abrupt_exit_recovers_atomic_records_without_undoing_acknowledged_history() {
    for (index, phase) in PHASES.into_iter().enumerate() {
        let tmp = Temp::new();
        let store = FileStore::new(&tmp.0).unwrap();
        let record = record();
        store.put(&record).unwrap();
        drop(store);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "records::tests::crash_writer"])
            .env("RDS_TEST_RECORD_CRASH_DIRECTORY", &tmp.0)
            .env("RDS_TEST_RECORD_CRASH_PHASE", index.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "{phase:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reopened = FileStore::new(&tmp.0).unwrap();
        assert_eq!(
            reopened.get(&key()).is_ok(),
            phase == Phase::BeforeDatabase,
            "{phase:?}"
        );
        if phase != Phase::BeforeDatabase {
            assert!(matches!(reopened.put(&record), Err(DiscoveryError::Stale)));
        }
    }
}

#[test]
fn record_observations_publish_after_commit_without_database_lock_or_io() {
    let tmp = Temp::new();
    let store = FileStore::new(&tmp.0).unwrap();
    let metrics = store.metrics().unwrap();
    let empty = metrics.snapshot().unwrap();
    assert_eq!(
        (empty.generation, empty.records, empty.identities),
        (Some(1), 0, 0)
    );
    let held = store.inner.lock().unwrap();
    assert_eq!(metrics.snapshot(), Some(empty));
    drop(held);
    let record = record();
    store
        .put_admitted(&record, &mut |_| {
            assert!(metrics.snapshot().is_none());
            Ok(())
        })
        .unwrap();
    let committed = metrics.snapshot().unwrap();
    assert_eq!(
        (
            committed.generation,
            committed.records,
            committed.identities
        ),
        (Some(2), 1, 1)
    );
    store.put(&record).unwrap();
    assert_eq!(metrics.snapshot(), Some(committed));
    let delete = DeleteRequest::new(&signer(), 2).unwrap();
    assert!(
        store
            .remove_admitted(&delete, &mut |_| Err(DiscoveryError::RateLimited))
            .is_err()
    );
    assert_eq!(metrics.snapshot(), Some(committed));
    store.remove(&delete).unwrap();
    let removed = metrics.snapshot().unwrap();
    assert_eq!(
        (removed.generation, removed.records, removed.identities),
        (Some(3), 0, 1)
    );
    store.remove(&delete).unwrap();
    assert_eq!(metrics.snapshot(), Some(removed));
    drop(store);
    assert!(metrics.snapshot().is_none());
    let reopened = FileStore::new(&tmp.0).unwrap();
    assert_eq!(reopened.metrics().unwrap().snapshot(), Some(removed));
}
