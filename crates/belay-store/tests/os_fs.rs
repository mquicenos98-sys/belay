//! `LogKv` on the real file system.

mod support;

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::thread;

use belay_common::fs::TempDir;
use belay_store::vfs::OsFs;
use belay_store::{inspect, KeyRange, Kv, LogKv, Options, StoreError, SyncPolicy, WriteBatch};

use support::{batch, dump, quiet_logs, states};

fn temp() -> TempDir {
    quiet_logs();
    TempDir::with_prefix("belay-store-test").expect("temp dir")
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn data_survives_close_and_reopen_on_disk() {
    let tmp = temp();
    let dir = tmp.join("store");
    let options = || {
        Options::new()
            .sync(SyncPolicy::Never)
            .segment_size(1024)
            .manual_checkpoints()
    };
    let kv = LogKv::open(&dir, options()).expect("open");
    assert!(dir.join("LOCK").exists());
    for i in 1..=40 {
        kv.write(batch(i)).expect("write");
    }
    kv.checkpoint().expect("checkpoint");
    for i in 41..=60 {
        kv.write(batch(i)).expect("write");
    }
    kv.close().expect("close");
    assert!(!dir.join("LOCK").exists(), "close releases the lock");

    let files = names(&dir);
    assert!(
        files.contains(&"snap-00000000000000000040.db".to_string()),
        "{files:?}"
    );
    assert!(
        files.contains(&"wal-00000000000000000041.log".to_string()),
        "{files:?}"
    );
    assert!(files
        .iter()
        .all(|n| n.starts_with("wal-") || n.starts_with("snap-")));

    let report = inspect::verify(&OsFs, &dir).expect("verify");
    assert!(report.is_ok(), "{report}");
    assert_eq!(report.last_seq, 60);

    let kv = LogKv::open(&dir, options()).expect("reopen");
    assert_eq!(kv.recovery().snapshot_seq, 40);
    assert_eq!(kv.recovery().batches_replayed, 20);
    assert_eq!(dump(&kv), states(60)[60]);
}

#[test]
fn a_second_open_of_the_same_directory_is_refused() {
    let tmp = temp();
    let kv = LogKv::open(tmp.path(), Options::new()).expect("open");
    match LogKv::open(tmp.path(), Options::new()) {
        Err(e @ StoreError::Locked { .. }) => {
            assert!(
                matches!(e, StoreError::Locked { pid: Some(pid), .. } if pid == std::process::id())
            );
            assert!(e.to_string().contains("delete the file"), "{e}");
        }
        other => panic!("expected Locked, got {other:?}"),
    }
    drop(kv);
    LogKv::open(tmp.path(), Options::new()).expect("open after the first closed");
}
