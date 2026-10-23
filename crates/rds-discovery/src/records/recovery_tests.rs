use super::tests::{Temp, key, record, signer};
use super::*;

#[test]
fn full_storage_anchor_retries_keep_locks_and_recover_exact_delete_history() {
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
        let original = record();
        store.put(&original).unwrap();
        store.inner.lock().unwrap().anchor.storage_full = Some(phase);
        let deletion = DeleteRequest::new(&signer(), 2).unwrap();
        assert!(matches!(
            store.remove(&deletion),
            Err(DiscoveryError::StorageUnavailable(_))
        ));
        for _ in 0..3 {
            assert!(matches!(
                store.get(&key()),
                Err(DiscoveryError::StorageUnavailable(_))
            ));
            assert!(!store.metrics().unwrap().snapshot().unwrap().healthy);
            assert!(
                FileStore::new(&tmp.0).is_err(),
                "recovery must retain ownership"
            );
        }
        store.inner.lock().unwrap().anchor.storage_full = None;
        assert!(matches!(store.get(&key()), Err(DiscoveryError::NotFound)));
        let snapshot = store.metrics().unwrap().snapshot().unwrap();
        assert!(snapshot.healthy);
        assert_eq!(snapshot.generation, Some(3));
        assert_eq!(snapshot.identities, 1);
        assert_eq!(snapshot.records, 0);
        assert!(matches!(store.put(&original), Err(DiscoveryError::Stale)));
        drop(store);
        let reopened = FileStore::new(&tmp.0).unwrap();
        assert!(matches!(
            reopened.get(&key()),
            Err(DiscoveryError::NotFound)
        ));
        assert!(matches!(
            reopened.put(&original),
            Err(DiscoveryError::Stale)
        ));
    }
}

#[test]
fn anchor_recovery_refuses_unrelated_history_and_stays_failed_closed() {
    let tmp = Temp::new();
    let store = FileStore::new(&tmp.0).unwrap();
    store.put(&record()).unwrap();
    store.inner.lock().unwrap().anchor.storage_full = Some(crate::persist::Phase::BeforeWrite);
    assert!(matches!(
        store.remove(&DeleteRequest::new(&signer(), 2).unwrap()),
        Err(DiscoveryError::StorageUnavailable(_))
    ));
    let previous = std::fs::read(tmp.0.join("records.anchor")).unwrap();
    let mut unrelated = decode_anchor(&previous).unwrap();
    unrelated.generation += 100;
    std::fs::write(
        tmp.0.join("records.anchor"),
        encode_anchor(&unrelated).unwrap(),
    )
    .unwrap();
    store.inner.lock().unwrap().anchor.storage_full = None;
    assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
    std::fs::write(tmp.0.join("records.anchor"), previous).unwrap();
    assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
    assert!(!store.metrics().unwrap().snapshot().unwrap().healthy);
}

#[test]
fn recovered_publication_exposes_only_exact_durable_bytes_once() {
    let tmp = Temp::new();
    let store = FileStore::new(&tmp.0).unwrap();
    let publication = record();
    store.inner.lock().unwrap().anchor.storage_full = Some(crate::persist::Phase::AfterRename);
    assert!(matches!(
        store.put(&publication),
        Err(DiscoveryError::StorageUnavailable(_))
    ));
    store.inner.lock().unwrap().anchor.storage_full = None;
    let recovered = store.get(&key()).unwrap();
    assert_eq!(recovered.payload, publication.payload);
    assert_eq!(recovered.signature, publication.signature);
    store.put(&publication).unwrap();
    let snapshot = store.metrics().unwrap().snapshot().unwrap();
    assert_eq!(snapshot.generation, Some(2));
    assert_eq!(snapshot.records, 1);
    assert_eq!(snapshot.identities, 1);
}

#[test]
fn anchor_recovery_refuses_changed_database_metadata() {
    let tmp = Temp::new();
    let store = FileStore::new(&tmp.0).unwrap();
    store.put(&record()).unwrap();
    store.inner.lock().unwrap().anchor.storage_full = Some(crate::persist::Phase::BeforeWrite);
    assert!(matches!(
        store.remove(&DeleteRequest::new(&signer(), 2).unwrap()),
        Err(DiscoveryError::StorageUnavailable(_))
    ));
    {
        let mut inner = store.inner.lock().unwrap();
        inner.anchor.storage_full = None;
        let mut unrelated = inner.pending_anchor.clone().unwrap();
        unrelated.wall_floor += Duration::from_secs(1);
        let txn = inner.db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(0, postcard::to_stdvec(&unrelated).unwrap().as_slice())
            .unwrap();
        txn.commit().unwrap();
    }
    assert!(matches!(store.get(&key()), Err(DiscoveryError::Store(_))));
    assert!(!store.metrics().unwrap().snapshot().unwrap().healthy);
}
