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
fn restore_searches_only_the_isolated_managed_cache() {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .env("REPRODB_HOME", home.path())
        .args([
            "restore",
            "sagatec",
            "--dump-id",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .assert()
        .failure()
        .code(50)
        .stdout(predicate::str::contains(
            "Locating the managed dump and validating the local target",
        ))
        .stderr(
            predicate::str::contains("managed dump artifact was not found")
                .and(predicate::str::contains("not implemented").not()),
        );
}

#[test]
fn restore_rejects_a_non_uuid_dump_id_as_usage() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["restore", "sagatec", "--dump-id", "../../dump.sql.zst"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "dump ID must be a canonical hyphenated UUID",
        ));
}

#[test]
fn setup_preview_is_safe_and_successful() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["setup", "--preview"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Local MySQL setup (preview)").and(
            predicate::str::contains("Docker and local configuration were not changed"),
        ));
}

#[test]
fn profile_add_preview_shows_the_planned_questions() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["profile", "add", "salt-source", "--preview"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Add source profile (preview)")
                .and(predicate::str::contains("MySQL host"))
                .and(predicate::str::contains("Salt Central"))
                .and(predicate::str::contains("nothing was saved")),
        );
}

#[test]
fn doctor_preview_never_claims_that_checks_were_executed() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["doctor", "--preview"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Environment ready for reprodb operations")
                .and(predicate::str::contains("were not executed")),
        );
}

#[test]
fn pull_preview_can_show_the_fresh_path() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["pull", "sagatec", "--fresh", "--preview"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Fresh dump requested")
                .and(predicate::str::contains("Domain     sagatec"))
                .and(predicate::str::contains("Source DB  salt_sagatec"))
                .and(predicate::str::contains("were not accessed")),
        );
}

#[test]
fn explicit_color_mode_adds_semantic_ansi_styles() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["pull", "sagatec", "--preview", "--color", "always"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\u{1b}[1;32m✓\u{1b}[0m")
                .and(predicate::str::contains("! Preview only")),
        );
}

#[test]
fn no_color_environment_keeps_preview_plain() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .env("NO_COLOR", "1")
        .args(["doctor", "--preview"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn preview_rejects_a_profile_name_that_could_control_the_terminal() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["profile", "add", "bad\nname", "--preview"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "profile name has an invalid format",
        ));
}

#[test]
fn errors_do_not_repeat_tenant_input() {
    let mut command = Command::cargo_bin("reprodb").unwrap();

    command
        .args(["dump", "sensitive-marker-must-not-leak;"])
        .assert()
        .failure()
        .code(2)
        .stderr(
            predicate::str::contains("tenant lookup has an invalid format")
                .and(predicate::str::contains("sensitive-marker").not()),
        );
}

#[test]
fn debug_logging_is_opt_in_and_does_not_repeat_arguments() {
    let mut default_command = Command::cargo_bin("reprodb").unwrap();
    default_command
        .args(["dump", "sensitive-marker-must-not-leak;"])
        .assert()
        .stderr(predicate::str::contains("command received").not());

    let mut debug_command = Command::cargo_bin("reprodb").unwrap();
    debug_command
        .env("RUST_LOG", "reprodb=debug")
        .args(["dump", "sensitive-marker-must-not-leak;"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("command received")
                .and(predicate::str::contains("command=\"dump\""))
                .and(predicate::str::contains("sensitive-marker").not()),
        );
}
