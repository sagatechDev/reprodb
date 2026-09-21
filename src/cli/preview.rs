use crate::{
    cli::output::OutputStyle,
    domain::{DatabaseName, ProfileName},
};

pub fn setup(style: &OutputStyle) -> String {
    let brand = style.brand("reprodb");
    let preview = style.attention("(preview)");
    let docker = style.section("Docker");
    let containers = style.section("MySQL containers found");
    let review = style.section("Review");
    let ok = style.success("✓");
    let selected = style.selected("›");
    let pending = style.attention("○");
    let warning = style.attention("! Port is exposed on every host interface");
    let footer =
        style.attention("! Preview only — Docker and local configuration were not changed.");

    format!(
        r#"{brand} · Local MySQL setup {preview}

{docker}
  {ok} Docker is available
  {ok} Context: desktop-linux

{containers}
  {selected} mysql-8  mysql:8  running  0.0.0.0:3306 → 3306/tcp
    Server detected: MySQL Community Server 8.4.4
    {warning}

? Use mysql-8 as the local restore target?  {yes}
? Local MySQL username                   {root}
? Local MySQL password                   ********

{review}
  Container   mysql-8
  Server      MySQL 8.4.4
  Address     localhost:3306
  Database    acme_production

{pending} Validate target connection
{pending} Save local target configuration

{footer}
"#,
        yes = style.value("Yes"),
        root = style.value("root"),
    )
}

pub fn profile_add(style: &OutputStyle, profile: &ProfileName) -> String {
    let brand = style.brand("reprodb");
    let preview = style.attention("(preview)");
    let profile_section = style.section("Profile");
    let connection = style.section("Source connection");
    let detection = style.section("Automatic detection");
    let database_selection = style.section("Database selection");
    let review = style.section("Review");
    let ok = style.success("✓");
    let pending = style.attention("○");
    let footer =
        style.attention("! Preview only — no connection was attempted and nothing was saved.");

    format!(
        r#"{brand} · Add source profile {preview}

{profile_section}
  Name        {profile}

{connection}
? MySQL host              mysql.salt.internal
? MySQL port              3306
? MySQL username          readonly_user
? MySQL password          ********
? Is this production?    No
? Connection security    Require TLS

{detection}
  {ok} Connected to the source
  {ok} MySQL Community Server 8.4.4 detected
  {ok} Approved MySQL 8.4.4 client selected

{database_selection}
  Input       Exact source database name
  Example     demo_acme

{review}
  Source      readonly_user@mysql.salt.internal:3306
  Server      MySQL 8.4.4
  Client      approved Docker image, pinned by digest
  Selection   Direct database
  Password    private local credential store

{pending} Save password under .reprodb with private permissions
{pending} Save profile and make it active

{footer}
"#
    )
}

pub fn doctor(style: &OutputStyle) -> String {
    let brand = style.brand("reprodb");
    let preview = style.attention("(preview)");
    let configuration = style.section("Configuration");
    let storage = style.section("Storage");
    let docker = style.section("Docker");
    let source = style.section("Source");
    let target = style.section("Local target");
    let ok = style.success("✓");
    let ready = style.success("✓ Environment ready for reprodb operations.");
    let footer = style.attention("! Preview only — checks are illustrative and were not executed.");

    format!(
        r#"{brand} doctor {preview}

{configuration}
  {ok} configuration
      configuration loaded and validated

{storage}
  {ok} cache filesystem
      120.0 GiB available for compressed dumps

{docker}
  {ok} Docker
      local context desktop-linux is available

{source}
  {ok} active profile
      salt-source · mysql.salt.internal:3306 · MySQL 8.4 · TLS REQUIRED
  {ok} source credential
      credential is available in the local store
  {ok} source connection
      server MySQL Community Server 8.4.4 · client 8.4.4 · TLS encrypted (TLS_AES_256_GCM_SHA384)

{target}
  {ok} local target configuration
      mysql-8 · context desktop-linux
  {ok} target credential
      credential is available in the local store
  {ok} target container identity
      configured name and full container ID match a running container
  {ok} target connection
      server MySQL Community Server 8.4.4 · client 8.4.4 · TLS encrypted (TLS_AES_256_GCM_SHA384)

{ready}

{footer}
"#
    )
}

pub fn pull(
    style: &OutputStyle,
    database: &DatabaseName,
    fresh: bool,
    target_container: Option<&crate::domain::ContainerName>,
    target_database: Option<&crate::domain::DatabaseName>,
) -> String {
    let database = database.as_str();
    let target_database = target_database.map_or(database, |database| database.as_str());
    let target_container = target_container.map_or("mysql-8", |container| container.as_str());
    let cache_message = if fresh {
        style.attention("! Fresh dump requested; the local cache will be ignored.")
    } else {
        "No valid local cache found.".to_owned()
    };
    let brand = style.brand("reprodb");
    let preview = style.attention("(preview)");
    let exporting = style.section("Exporting source database");
    let restoring = style.section("Restoring local database");
    let ok = style.success("✓");
    let ready = style.success("Ready.");
    let footer = style
        .attention("! Preview only — source, cache, Docker and local databases were not accessed.");
    let profile = style.value("salt-source");
    let source_value = style.value(database);
    let target_value = style.value(&format!("{target_container}/{target_database}"));

    format!(
        r#"{brand} pull {preview}

Profile    {profile}
Source DB  {source_value}
Target DB  {target_value}

{cache_message}

{exporting}
1.84 GiB | 42.1 MiB/s | 00:44 | ETA ~00:18

{restoring}
  {ok} database recreated with source charset and collation
  {ok} import completed

{ready}

Database   {target_database}
Container  {target_container}

{footer}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_preview_uses_observed_salt_examples_and_is_side_effect_free() {
        let profile = ProfileName::try_from("salt-source").unwrap();
        let output = profile_add(&OutputStyle::plain(), &profile);

        assert!(output.contains("Profile\n  Name        salt-source"));
        assert!(output.contains("Exact source database name"));
        assert!(output.contains("demo_acme"));
        assert!(output.contains("MySQL password          ********"));
        assert!(output.contains("nothing was saved"));
        assert!(!output.contains("readonly_password"));
    }

    #[test]
    fn fresh_pull_preview_resolves_the_observed_acme_database() {
        let database = DatabaseName::try_from("acme_production").unwrap();
        let output = pull(&OutputStyle::plain(), &database, true, None, None);

        assert!(output.contains("Fresh dump requested"));
        assert!(output.contains("Source DB  acme"));
        assert!(output.contains("Target DB  mysql-8/acme"));
        assert!(output.contains("were not accessed"));
    }

    #[test]
    fn preview_uses_the_supplied_database_name_without_inventing_a_prefix() {
        let database = DatabaseName::try_from("unknown_database").unwrap();
        let output = pull(&OutputStyle::plain(), &database, false, None, None);

        assert!(output.contains("Source DB  unknown"));
        assert!(!output.contains("salt_unknown"));
    }

    #[test]
    fn colored_preview_keeps_text_indicators_in_addition_to_color() {
        let output = setup(&OutputStyle::colored());

        assert!(output.contains("\u{1b}["));
        assert!(output.contains("✓"));
        assert!(output.contains("Docker is available"));
        assert!(output.contains("! Port is exposed"));
        assert!(output.contains("! Preview only"));
    }

    #[test]
    fn doctor_preview_matches_the_real_report_shape() {
        let output = doctor(&OutputStyle::plain());

        for section in [
            "Configuration",
            "Storage",
            "Docker",
            "Source",
            "Local target",
        ] {
            assert!(output.contains(section));
        }
        assert!(output.contains("client 8.4.4"));
        assert!(output.contains("TLS_AES_256_GCM_SHA384"));
        assert!(output.contains("checks are illustrative"));
    }
}
