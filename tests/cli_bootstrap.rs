use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_is_available() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Local CLI for reproducing Salt tenant databases",
        ));
}

#[test]
fn version_is_available() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("reprodb "));
}
