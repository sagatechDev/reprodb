use crate::{
    cli::output::OutputStyle,
    domain::{ProfileName, TenantLookup},
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
  Database    salt_sagatec (example resolved from tenant)

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
    let tenant_resolution = style.section("Tenant resolution");
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

{tenant_resolution}
  Resolver    Salt Central
  Database    salt_central
  Examples    sagatec → salt_sagatec; polymer → salt_polymer

{review}
  Source      readonly_user@mysql.salt.internal:3306
  Server      MySQL 8.4.4
  Client      approved Docker image, pinned by digest
  Resolver    Salt Central
  Password    OS credential store

{pending} Save password in Keychain / Secret Service
{pending} Save profile and make it active

{footer}
"#
    )
}

pub fn doctor(style: &OutputStyle) -> String {
    let brand = style.brand("reprodb");
    let preview = style.attention("(preview)");
    let configuration = style.section("Configuration");
    let client = style.section("MySQL client");
    let target = style.section("Local target");
    let source = style.section("Source");
    let ok = style.success("✓");
    let ready = style.success("Ready to pull a tenant.");
    let footer = style.attention("! Preview only — checks are illustrative and were not executed.");

    format!(
        r#"{brand} doctor {preview}

{configuration}
  {ok} configuration file
  {ok} active profile: salt-source
  {ok} source credential available

{client}
  {ok} approved Docker image available
  {ok} mysql client: 8.4.4
  {ok} mysqldump: 8.4.4

{target}
  {ok} Docker context: desktop-linux
  {ok} container: mysql-8 (running)
  {ok} target connection

{source}
  {ok} source connection
  {ok} server version: 8.4.4
  {ok} tenant resolver: salt_central

{ready}

{footer}
"#
    )
}

pub fn pull(style: &OutputStyle, tenant: &TenantLookup, fresh: bool) -> String {
    let (tenant_id, database) = preview_resolution(tenant);
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
    let tenant_value = style.value(tenant.as_str());
    let source_value = style.value(database);
    let target_value = style.value(&format!("mysql-8/{database}"));

    format!(
        r#"{brand} pull {preview}

Profile    {profile}
Domain     {tenant_value}
Tenant ID  {tenant_id}
Source DB  {source_value}
Target DB  {target_value}

{cache_message}

{exporting}
1.84 GiB | 42.1 MiB/s | 00:44

{restoring}
  {ok} database recreated with source charset and collation
  {ok} import completed
  {ok} local tenant registration updated

{ready}

Database   {database}
Container  mysql-8

{footer}
"#
    )
}

fn preview_resolution(tenant: &TenantLookup) -> (&str, &str) {
    match tenant.as_str() {
        "sagatec" => ("salt_sagatec", "salt_sagatec"),
        "polymer" => ("salt_polymer", "salt_polymer"),
        _ => ("<resolved-tenant-id>", "<resolved-database>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_preview_uses_observed_salt_examples_and_is_side_effect_free() {
        let profile = ProfileName::try_from("salt-source").unwrap();
        let output = profile_add(&OutputStyle::plain(), &profile);

        assert!(output.contains("Profile\n  Name        salt-source"));
        assert!(output.contains("sagatec → salt_sagatec"));
        assert!(output.contains("polymer → salt_polymer"));
        assert!(output.contains("MySQL password          ********"));
        assert!(output.contains("nothing was saved"));
        assert!(!output.contains("readonly_password"));
    }

    #[test]
    fn fresh_pull_preview_resolves_the_observed_sagatec_database() {
        let tenant = TenantLookup::try_from("sagatec").unwrap();
        let output = pull(&OutputStyle::plain(), &tenant, true);

        assert!(output.contains("Fresh dump requested"));
        assert!(output.contains("Source DB  salt_sagatec"));
        assert!(output.contains("Target DB  mysql-8/salt_sagatec"));
        assert!(output.contains("were not accessed"));
    }

    #[test]
    fn preview_does_not_invent_a_resolution_for_an_unknown_domain() {
        let tenant = TenantLookup::try_from("unknown").unwrap();
        let output = pull(&OutputStyle::plain(), &tenant, false);

        assert!(output.contains("Source DB  <resolved-database>"));
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
}
