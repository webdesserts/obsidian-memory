use chrono::{Datelike, IsoWeek, Local};
use rmcp::model::{CallToolResult, Content, RawResource};

/// Get the current weekly note URI and metadata.
///
/// Returns a ResourceLink to the current week's journal note using
/// ISO 8601 week date format (e.g., memory:journal/2025-w01).
pub fn execute() -> Result<CallToolResult, rmcp::model::ErrorData> {
    let now = Local::now();
    let iso_week: IsoWeek = now.iso_week();
    let week = iso_week.week();
    let year = iso_week.year();

    // Day of week (Monday, Tuesday, etc.)
    let current_day = match now.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };

    // Weekly note URI format: memory:journal/YYYY-wWW
    let weekly_note_uri = format!("memory:journal/{}-w{:02}", year, week);

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
