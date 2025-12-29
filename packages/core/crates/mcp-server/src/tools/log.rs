use chrono::{DateTime, Datelike, Local, Timelike};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;
use tokio::fs;

/// Format ISO week date as YYYY-Www-D (e.g., 2025-W48-1)
/// Uses chrono's IsoWeek trait
fn format_iso_week_date(dt: &DateTime<Local>) -> String {
    let iso_week = dt.iso_week();
    let weekday = dt.weekday().num_days_from_monday() + 1; // 1=Mon, 7=Sun
    format!("{}-W{:02}-{}", iso_week.year(), iso_week.week(), weekday)
}

/// Get 3-letter day abbreviation (Mon, Tue, etc.)
fn get_day_abbreviation(dt: &DateTime<Local>) -> &'static str {
    match dt.weekday() {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

/// Format time as 12-hour clock (h:MM AM/PM)
fn format_12_hour_time(dt: &DateTime<Local>) -> String {
    let hour = dt.hour();
    let minute = dt.minute();
    let (hour_12, am_pm) = if hour == 0 {
        (12, "AM")
    } else if hour < 12 {
        (hour, "AM")
    } else if hour == 12 {
        (12, "PM")
    } else {
        (hour - 12, "PM")
    };
    format!("{}:{:02} {}", hour_12, minute, am_pm)
}

/// Parse time from a log entry line (e.g., "- 9:30 AM – content")
/// Returns (hour, minute) in 24-hour format, or None if parsing fails
fn parse_entry_time(entry: &str) -> Option<(u32, u32)> {
    // Match pattern: "- TIME – " where TIME is like "9:30 AM"
    let entry = entry.strip_prefix("- ")?;
    let time_end = entry.find(" – ")?;
    let time_str = &entry[..time_end];

    // Parse "h:mm AM" or "h:mm PM"
    let parts: Vec<&str> = time_str.split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }

    let time_parts: Vec<&str> = parts[0].split(':').collect();
    if time_parts.len() != 2 {
        return None;
    }

    let hour: u32 = time_parts[0].parse().ok()?;
    let minute: u32 = time_parts[1].parse().ok()?;
    let am_pm = parts[1];

    let hour_24 = match am_pm {
        "AM" if hour == 12 => 0,
        "AM" => hour,
        "PM" if hour == 12 => 12,
        "PM" => hour + 12,
        _ => return None,
    };

    Some((hour_24, minute))
}

/// Add a new entry to the log file, organizing by day and sorting chronologically
pub async fn add_log(
    log_path: &Path,
    time: DateTime<Local>,
    entry: &str,
) -> Result<(String, String), std::io::Error> {
    let iso_week_date = format_iso_week_date(&time);
    let day_abbrev = get_day_abbreviation(&time);
    let time_str = format_12_hour_time(&time);

    // Format the new entry - strip leading dash if present
    let bullet_content = entry.strip_prefix('-').map(|s| s.trim()).unwrap_or(entry);
    let new_entry = format!("- {} – {}", time_str, bullet_content);

    // Read existing log content
    let log_content = match fs::read_to_string(log_path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    // Parse log into lines
    let day_header = format!("## {} ({})", iso_week_date, day_abbrev);
    let mut lines: Vec<String> = log_content.lines().map(String::from).collect();

    // Find the section for this day
    let section_index = lines.iter().position(|l| l == &day_header);

    if section_index.is_none() {
        // Day section doesn't exist - add at the end
        if !lines.is_empty() {
            lines.push(String::new()); // blank line before new section
        }
        lines.push(day_header);
        lines.push(String::new());
        lines.push(new_entry);
        lines.push(String::new()); // trailing newline
    } else {
        let section_start = section_index.unwrap();

        // Find all entries in this section and their times
        let mut entries: Vec<(usize, Option<(u32, u32)>)> = Vec::new();
        let mut current_index = section_start + 1;

        // Skip blank lines after header
        while current_index < lines.len() && lines[current_index].trim().is_empty() {
            current_index += 1;
        }

        // Collect all entries in this section
        while current_index < lines.len() && !lines[current_index].starts_with("##") {
            let line = &lines[current_index];
            if line.starts_with('-') {
                entries.push((current_index, parse_entry_time(line)));
            }
            current_index += 1;
        }

        // Find where to insert based on time
        let new_time = (time.hour(), time.minute());
        let mut insert_index = section_start + 1;

        // Skip blank lines after header
        while insert_index < lines.len() && lines[insert_index].trim().is_empty() {
            insert_index += 1;
        }

        // Find chronological position
        for (entry_idx, entry_time) in &entries {
            if let Some(existing_time) = entry_time {
                if *existing_time > new_time {
                    insert_index = *entry_idx;
                    break;
                }
            }
            insert_index = entry_idx + 1;
        }

        // Insert the new entry
        lines.insert(insert_index, new_entry);
    }

    // Write the file
    let content = lines.join("\n");
    fs::write(log_path, content).await?;

    Ok((iso_week_date, time_str))
}

/// Execute the Log tool
pub async fn execute(vault_path: &Path, content: &str) -> Result<CallToolResult, ErrorData> {
    let log_path = vault_path.join("Log.md");
    let now = Local::now();

    match add_log(&log_path, now, content).await {
        Ok((iso_week_date, time_str)) => {
            let text = format!("Logged at {} {}", iso_week_date, time_str);
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
        Err(e) => Err(ErrorData::internal_error(
            format!("Failed to write log: {}", e),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn make_time(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2025, 12, 29, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn test_format_iso_week_date() {
        let dt = make_time(10, 0);
        let result = format_iso_week_date(&dt);
        // 2025-12-29 is Monday of week 1 of 2026 (ISO week dating)
        assert!(result.starts_with("2026-W01-1") || result.starts_with("2025-W52-"));
    }

    #[test]
    fn test_format_12_hour_time() {
        assert_eq!(format_12_hour_time(&make_time(0, 30)), "12:30 AM");
        assert_eq!(format_12_hour_time(&make_time(9, 5)), "9:05 AM");
        assert_eq!(format_12_hour_time(&make_time(12, 0)), "12:00 PM");
        assert_eq!(format_12_hour_time(&make_time(15, 30)), "3:30 PM");
        assert_eq!(format_12_hour_time(&make_time(23, 59)), "11:59 PM");
    }

    #[test]
    fn test_parse_entry_time() {
        assert_eq!(parse_entry_time("- 9:30 AM – some content"), Some((9, 30)));
        assert_eq!(
            parse_entry_time("- 12:00 PM – afternoon entry"),
            Some((12, 0))
        );
        assert_eq!(parse_entry_time("- 3:45 PM – later entry"), Some((15, 45)));
        assert_eq!(parse_entry_time("- 12:30 AM – midnight entry"), Some((0, 30)));
        assert_eq!(parse_entry_time("not a valid entry"), None);
    }

    #[tokio::test]
    async fn test_add_log_creates_file_if_not_exists() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("Log.md");

        let time = make_time(10, 30);
        let (iso_week_date, time_str) = add_log(&log_path, time, "Test entry").await.unwrap();

        assert!(!iso_week_date.is_empty());
        assert_eq!(time_str, "10:30 AM");

        let content = fs::read_to_string(&log_path).await.unwrap();
        assert!(content.contains("Test entry"));
        assert!(content.contains("10:30 AM"));
    }

    #[tokio::test]
    async fn test_add_log_appends_to_existing_section() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("Log.md");

        let time1 = make_time(9, 0);
        add_log(&log_path, time1, "First entry").await.unwrap();

        let time2 = make_time(11, 0);
        add_log(&log_path, time2, "Second entry").await.unwrap();

        let content = fs::read_to_string(&log_path).await.unwrap();
        assert!(content.contains("First entry"));
        assert!(content.contains("Second entry"));

        // Verify order - first entry should come before second
        let first_pos = content.find("First entry").unwrap();
        let second_pos = content.find("Second entry").unwrap();
        assert!(first_pos < second_pos);
    }

    #[tokio::test]
    async fn test_add_log_maintains_chronological_order() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("Log.md");

        // Add entries out of order
        let time2 = make_time(14, 0);
        add_log(&log_path, time2, "Afternoon entry").await.unwrap();

        let time1 = make_time(9, 0);
        add_log(&log_path, time1, "Morning entry").await.unwrap();

        let content = fs::read_to_string(&log_path).await.unwrap();

        // Morning should come before afternoon
        let morning_pos = content.find("Morning entry").unwrap();
        let afternoon_pos = content.find("Afternoon entry").unwrap();
        assert!(
            morning_pos < afternoon_pos,
            "Morning entry should come before afternoon entry"
        );
    }
}
