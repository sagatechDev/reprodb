use crate::{application::TenantCatalogPage, cli::output::OutputStyle};

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} Reading the tenant catalog without modifying the source...\n",
        style.selected("○")
    )
}

pub fn render_page(style: &OutputStyle, page: &TenantCatalogPage) -> String {
    let mut output = format!(
        "\n{} · Tenants\n\nProfile   {}\nCentral   {}\nFound     {}{}\n\n",
        style.brand("reprodb"),
        style.value(&page.profile_name),
        style.value(page.central_database.as_str()),
        page.entries.len(),
        if page.truncated { "+" } else { "" }
    );
    if page.entries.is_empty() {
        output.push_str("No tenants were returned by this central database.\n");
        return output;
    }
    for entry in &page.entries {
        output.push_str(&format!(
            "{} {}\n    Database  {}\n    Domain    {}\n",
            style.value("›"),
            entry.tenant_id,
            entry.database,
            entry.primary_domain.as_deref().unwrap_or("—")
        ));
    }
    if page.truncated {
        output.push_str("\n! More tenants exist. Increase --limit (maximum 500).\n");
    }
    output
}

#[cfg(test)]
mod tests {
    use crate::{
        application::{TenantCatalogEntry, TenantCatalogPage},
        cli::output::OutputStyle,
        domain::{DatabaseName, TenantId},
    };

    use super::*;

    #[test]
    fn renders_only_safe_tenant_catalog_fields_and_truncation() {
        let page = TenantCatalogPage {
            profile_name: "sandbox".to_owned(),
            central_database: DatabaseName::try_from("demo_central").unwrap(),
            entries: vec![TenantCatalogEntry {
                tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                primary_domain: Some("sagatec".to_owned()),
            }],
            truncated: true,
        };

        let output = render_page(&OutputStyle::plain(), &page);

        assert!(output.contains("Profile   sandbox"));
        assert!(output.contains("Central   demo_central"));
        assert!(output.contains("Database  salt_sagatec"));
        assert!(output.contains("Domain    sagatec"));
        assert!(output.contains("Increase --limit"));
    }
}
