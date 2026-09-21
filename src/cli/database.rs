use crate::{application::DatabaseCatalogPage, cli::output::OutputStyle};

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} Reading source database names without modifying the server...\n",
        style.selected("○")
    )
}

pub fn render_page(style: &OutputStyle, page: &DatabaseCatalogPage) -> String {
    let mut output = format!(
        "\n{} · Source databases\n\nProfile   {}\nFound     {}{}\n\n",
        style.brand("reprodb"),
        style.value(&page.profile_name),
        page.entries.len(),
        if page.truncated { "+" } else { "" }
    );
    if page.entries.is_empty() {
        output.push_str("No user databases were returned by this source.\n");
        return output;
    }
    for entry in &page.entries {
        output.push_str(&format!("{} {}\n", style.value("›"), entry.database));
    }
    if page.truncated {
        output.push_str("\n! More databases exist. Increase --limit (maximum 500).\n");
    }
    output
}

#[cfg(test)]
mod tests {
    use crate::{
        application::{DatabaseCatalogEntry, DatabaseCatalogPage},
        cli::output::OutputStyle,
    };

    use super::*;

    #[test]
    fn renders_only_safe_database_catalog_fields_and_truncation() {
        let page = DatabaseCatalogPage {
            profile_name: "sandbox".to_owned(),
            entries: vec![DatabaseCatalogEntry {
                database: "demo_acme".to_owned(),
            }],
            truncated: true,
        };

        let output = render_page(&OutputStyle::plain(), &page);

        assert!(output.contains("Profile   sandbox"));
        assert!(output.contains("Source databases"));
        assert!(output.contains("demo_acme"));
        assert!(output.contains("Increase --limit"));
    }
}
