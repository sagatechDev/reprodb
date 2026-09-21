#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    fs,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use reprodb::{
    domain::{DatabaseName, ProfileName},
    infrastructure::operation_lock::{
        OperationLockError, OperationLockKey, OperationLockManager, OperationLockScope,
    },
};
use tempfile::tempdir;

const CHILD_ENV: &str = "REPRODB_OPERATION_LOCK_CHILD";
const ROOT_ENV: &str = "REPRODB_OPERATION_LOCK_ROOT";
const READY_FILE: &str = "child-ready";

fn key() -> OperationLockKey {
    OperationLockKey::source(
        &ProfileName::try_from("local-source").unwrap(),
        &DatabaseName::try_from("acme_production").unwrap(),
    )
}

#[test]
fn operation_lock_is_exclusive_between_processes_and_recovers_after_crash() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let root = std::env::var_os(ROOT_ENV).expect("child lock root must be configured");
        let manager = OperationLockManager::new(&root);
        let _guard = manager.try_acquire(key()).unwrap();
        fs::write(std::path::Path::new(&root).join(READY_FILE), b"ready").unwrap();
        thread::sleep(Duration::from_secs(30));
        return;
    }

    let directory = tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("operation_lock_is_exclusive_between_processes_and_recovers_after_crash")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(ROOT_ENV, directory.path())
        .spawn()
        .unwrap();
    wait_until_child_holds_lock(&mut child, directory.path().join(READY_FILE));

    let manager = OperationLockManager::new(directory.path());
    let was_busy = matches!(
        manager.try_acquire(key()),
        Err(OperationLockError::Busy {
            scope: OperationLockScope::Source
        })
    );

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "the child should have been terminated");
    assert!(was_busy, "a different process must observe the held lock");
    assert!(
        manager.try_acquire(key()).is_ok(),
        "the OS must release the advisory lock after process termination"
    );
}

fn wait_until_child_holds_lock(child: &mut Child, ready: std::path::PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ready.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("lock child exited before becoming ready: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("lock child did not become ready within five seconds");
}
