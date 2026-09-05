use crate::domain::{ProfileName, TenantLookup};

pub fn setup() -> String {
    r#"reprodb · Local MySQL setup (preview)

Docker
  ✓ Docker is available
  ✓ Context: desktop-linux

MySQL containers found
  › mysql-8       mysql:8.4    running    localhost:3306
    mysql-legacy  mysql:5.7    stopped    localhost:3307

? Use mysql-8 as the local restore target?  Yes
? Local MySQL username  root
? Local MySQL password  ••••••••

Review
  Container   mysql-8
  Image       mysql:8.4
  Address     localhost:3306
  Database    resolved from the tenant

○ Validate target connection
○ Save local target configuration

Preview only — Docker and local configuration were not changed.
"#
    .to_owned()
}

pub fn profile_add(profile: &ProfileName) -> String {
    format!(
        r#"reprodb · Add source profile (preview)

Profile
  Name        {profile}

Connection
? MySQL host              db.example.internal
? MySQL port              3306
? MySQL username          readonly_user
? MySQL password          ••••••••
? MySQL server series     8.4
? Require TLS             Yes

Tenant resolution
? Resolver                Salt Central
? Central database        salt_central
? Domain lookup column    domain

Review
  Source      readonly_user@db.example.internal:3306
  MySQL       8.4 (client selected from the approved catalog)
  Resolver    Salt Central
  Password    OS credential store

○ Test source connection
○ Save password in Keychain / Secret Service
○ Save profile and make it active

Preview only — no connection was attempted and nothing was saved.
"#
    )
}

pub fn doctor() -> String {
    r#"reprodb doctor (preview)

Configuration
  ✓ configuration file
  ✓ active profile: salt-local
  ✓ source credential available

MySQL client
  ✓ Docker image approved for MySQL 8.4
  ✓ mysql client: 8.4.4
  ✓ mysqldump: 8.4.4

Local target
  ✓ Docker context: desktop-linux
  ✓ container: mysql-8 (running)
  ✓ target connection

Source
  ✓ source connection
  ✓ server version: 8.4.4

Ready to pull a tenant.

Preview only — checks above are illustrative and were not executed.
"#
    .to_owned()
}

pub fn pull(tenant: &TenantLookup, fresh: bool) -> String {
    let cache_message = if fresh {
        "Fresh dump requested; the local cache will be ignored."
    } else {
        "No valid local cache found."
    };

    format!(
        r#"reprodb pull (preview)

Profile   salt-local
Tenant    {tenant}
Source    resolved through salt_central
Target    mysql-8/<resolved-database>

Checking cache...
{cache_message}

Exporting...
1.84 GiB | 42.1 MiB/s | 00:44

Restoring...
✓ local database recreated
✓ import completed

Ready.

Database   <resolved-database>
Container  mysql-8

Preview only — source, cache, Docker and local databases were not accessed.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_preview_is_explicitly_side_effect_free() {
        let profile = ProfileName::try_from("salt-local").unwrap();
        let output = profile_add(&profile);

        assert!(output.contains("Profile\n  Name        salt-local"));
        assert!(output.contains("nothing was saved"));
        assert!(!output.contains("readonly_password"));
    }

    #[test]
    fn fresh_pull_preview_explains_that_cache_is_ignored() {
        let tenant = TenantLookup::try_from("guerra").unwrap();
        let output = pull(&tenant, true);

        assert!(output.contains("Fresh dump requested"));
        assert!(output.contains("were not accessed"));
    }
}
