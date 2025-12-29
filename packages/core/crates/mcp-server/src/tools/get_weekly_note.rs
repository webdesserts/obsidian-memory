use std::path::Path;

use chrono::{Datelike, IsoWeek, Local};
use rmcp::model::{CallToolResult, Content, RawResource};

/// Get the current ISO week date string and day name.
///
/// Returns (iso_week_date, day_name) where iso_week_date is like "2025-W01".
pub fn get_current_week_info() -> (String, &'static str) {
    let now = Local::now();
    let iso_week: IsoWeek = now.iso_week();
    let week = iso_week.week();
    let year = iso_week.year();

    let iso_week_date = format!("{}-W{:02}", year, week);

    let day_name = match now.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };

    (iso_week_date, day_name)
}

/// Format the weekly note URI given a vault path and ISO week date.
///
/// Returns a file:// URI pointing to the weekly note file.
pub fn format_weekly_note_uri(vault_path: &Path, iso_week_date: &str) -> String {
    let file_path = vault_path.join(format!("journal/{}.md", iso_week_date.to_lowercase()));
    format!("file://{}", file_path.display())
}

/// Get the current weekly note URI and metadata.
///
/// Returns a ResourceLink to the current week's journal note using
/// ISO 8601 week date format (e.g., memory:journal/2025-w01).
pub fn execute() -> Result<CallToolResult, rmcp::model::ErrorData> {
    let (iso_week_date, current_day) = get_current_week_info();

    // Weekly note URI format: memory:journal/YYYY-wWW
    let weekly_note_uri = format!("memory:journal/{}", iso_week_date.to_lowercase());

    // Parse year and week from iso_week_date for display
    let parts: Vec<&str> = iso_week_date.split('-').collect();
    let year = parts[0];
    let week = parts[1].trim_start_matches('W');

    // Return ResourceLink to the weekly note
    let resource = RawResource {
        uri: weekly_note_uri,
        name: format!("{} Week {}", year, week),
        title: None,
        description: Some(format!("Current weekly journal ({})", current_day)),
        mime_type: Some("text/markdown".to_string()),
        size: None,
        icons: None,
        meta: None,
    };

    Ok(CallToolResult::success(vec![Content::resource_link(
        resource,
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_returns_resource_link() {
        let result = execute().expect("should succeed");
        assert!(!result.is_error.unwrap_or(false));
        assert!(!result.content.is_empty());

        // Check that we got a resource link
        let content = &result.content[0];
        let link = content.raw.as_resource_link().expect("Expected ResourceLink");

        // URI should start with memory:journal/
        assert!(link.uri.starts_with("memory:journal/"));
        // Should have -w followed by week number
        assert!(link.uri.contains("-w"));
        // Name should contain "Week"
        assert!(link.name.contains("Week"));
    }

    #[test]
    fn test_uri_format() {
        let result = execute().expect("should succeed");
        let content = &result.content[0];
        let link = content.raw.as_resource_link().expect("Expected ResourceLink");

        // Format should be memory:journal/YYYY-wWW
        let uri = &link.uri;
        let parts: Vec<&str> = uri.split('/').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "memory:journal");

        // Second part should be YYYY-wWW
        let date_part = parts[1];
        assert!(date_part.len() == 8); // YYYY-wWW
        assert!(date_part.contains("-w"));
    }
}
