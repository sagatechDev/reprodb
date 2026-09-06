use std::io::Write as _;

use crate::{
    application::{RestorePlan, RestoreProgress, RestoreProgressObserver, RestoreReady},
    cli::{dump::format_bytes, output::OutputStyle},
};

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} restore\n{} Locating the managed dump and validating the local target...\n",
        style.brand("reprodb"),
        style.attention("○"),
    )
}

pub fn render_plan(style: &OutputStyle, plan: &RestorePlan) -> String {
    format!(
        "\n{}\n  Source:     {}\n  Lookup:     {}\n  Tenant ID:  {}\n  Database:   {}\n  Dump ID:    {}\n  MySQL:      {} (client {})\n  Target:     {}/{}\n  Domain:     {}\n\n{} The configured local tenant database will be replaced.\n",
        style.section("Restore plan"),
        style.value(plan.profile.as_str()),
        style.value(plan.tenant_lookup.as_str()),
        style.value(plan.tenant_id.as_str()),
        style.value(plan.database.as_str()),
        style.value(&plan.dump_id.to_string()),
        plan.source_version,
        plan.client_version,
        style.value(plan.container.as_str()),
        style.value(plan.database.as_str()),
        style.value(plan.local_domain.as_str()),
        style.attention("!"),
    )
}

pub fn render_complete(style: &OutputStyle, restored: &RestoreReady) -> String {
    let plan = &restored.plan;
    format!(
        "\n{} Restore ready\n\n  Database:   {}\n  Container:  {}\n  Domain:     {}\n  Imported:   {}\n  Dump ID:    {}\n",
        style.success("✓"),
        style.value(plan.database.as_str()),
        style.value(plan.container.as_str()),
        style.value(plan.local_domain.as_str()),
        format_bytes(restored.imported_bytes),
        style.value(&plan.dump_id.to_string()),
    )
}

pub struct CliRestoreProgress {
    style: OutputStyle,
}

impl CliRestoreProgress {
    pub const fn new(style: OutputStyle) -> Self {
        Self { style }
    }
}

impl RestoreProgressObserver for CliRestoreProgress {
    fn update(&self, progress: &RestoreProgress) {
        match progress {
            RestoreProgress::PlanReady(plan) => print!("{}", render_plan(&self.style, plan)),
            RestoreProgress::RestoringDatabase => println!(
                "{} Recreating the database and streaming the validated dump...",
                self.style.attention("○")
            ),
            RestoreProgress::RegisteringTenant => {
                println!("{} Database import completed", self.style.success("✓"));
                println!(
                    "{} Updating the local Salt tenant registration...",
                    self.style.attention("○")
                );
            }
        }
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        ContainerName, DatabaseName, DomainAlias, DumpId, MysqlVersion, ProfileName, TenantId,
        TenantLookup,
    };

    use super::*;

    fn plan() -> RestorePlan {
        RestorePlan {
            profile: ProfileName::try_from("salt-local").unwrap(),
            tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
            tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
            database: DatabaseName::try_from("salt_sagatec").unwrap(),
            dump_id: DumpId::new(),
            source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            container: ContainerName::try_from("mysql-8").unwrap(),
            local_domain: DomainAlias::try_from("sagatec").unwrap(),
        }
    }

    #[test]
    fn plan_makes_the_destructive_local_scope_visible() {
        let output = render_plan(&OutputStyle::plain(), &plan());

        assert!(output.contains("Source:     salt-local"));
        assert!(output.contains("Target:     mysql-8/salt_sagatec"));
        assert!(output.contains("configured local tenant database will be replaced"));
        assert!(!output.to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn completion_reports_the_local_domain_without_inventing_an_http_address() {
        let restored = RestoreReady {
            plan: plan(),
            imported_bytes: 5 * 1024 * 1024,
        };

        let output = render_complete(&OutputStyle::plain(), &restored);

        assert!(output.contains("✓ Restore ready"));
        assert!(output.contains("Domain:     sagatec"));
        assert!(!output.contains(".localhost"));
        assert!(output.contains("Imported:   5.0 MiB"));
    }
}
