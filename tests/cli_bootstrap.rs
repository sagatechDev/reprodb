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

#[test]
fn root_help_lists_the_command_tree() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command.arg("--help").assert().success().stdout(
        predicate::str::contains("setup")
            .and(predicate::str::contains("profile"))
            .and(predicate::str::contains("doctor"))
            .and(predicate::str::contains("dump"))
            .and(predicate::str::contains("restore"))
            .and(predicate::str::contains("pull"))
            .and(predicate::str::contains("cache")),
    );
}

#[test]
fn unknown_command_is_a_usage_error() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("unknown")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn dump_requires_a_tenant() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("dump")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("<TENANT>"));
}

#[test]
fn restore_requires_a_managed_dump_id() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["restore", "sagatec"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--dump-id <ID>"));
}

#[test]
fn profile_requires_a_subcommand() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("profile")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("Usage: reprodb profile"));
}

#[test]
fn cache_requires_a_subcommand() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("cache")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("Usage: reprodb cache"));
}

#[test]
fn valid_but_unimplemented_command_fails_explicitly() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .arg("setup")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "command `setup` is not implemented yet",
        ));
}
