Feature: Log and WriteLogs tools
  The Log tool lets agents append timestamped entries to a session log
  (Log.md), automatically organized by day. WriteLogs lets agents replace
  or delete an entire day's entries during memory consolidation.

  Background:
    Given a vault with a Log.md file

  # --- Appending entries ---

  Scenario: Logging the first entry creates the file
    When the agent logs "Started work"
    Then Log.md exists with one day section for today
    And the section contains "Started work" tagged with the current time

  Scenario: Entries within a day stay in chronological order
    Given someone logged "Afternoon task" at 2:00 PM
    When the agent logs "Morning task" at 9:00 AM
    Then "Morning task" appears before "Afternoon task"

  Scenario: Different days get their own sections
    Given an entry exists for Monday
    When the agent logs an entry on Tuesday
    Then Log.md has separate sections for Monday and Tuesday

  # --- Replacing entries ---

  Scenario: Replacing a day's entries
    Given Monday has entries at 9:00 AM and 2:00 PM
    When the agent replaces Monday's entries with a single entry at 10:00 AM
    Then Monday's section contains only the 10:00 AM entry

  Scenario: Deleting a day's section
    Given Monday has log entries
    When the agent replaces Monday with no entries
    Then Monday's section is removed entirely

  Scenario: Replacement entries are sorted automatically
    When the agent writes entries at 3:00 PM, 9:00 AM, and 12:00 PM
    Then they appear in chronological order

  # --- Healing duplicate sections ---
  #
  # P2P sync can merge text at the CRDT level without markdown awareness,
  # creating duplicate day headers. Both tools heal these on contact.

  Scenario: Logging into a file with duplicate day sections merges them
    Given Log.md has two "## 2026-W09-1 (Mon)" sections
    And the first has "- 9:00 AM – Morning task"
    And the second has "- 2:00 PM – Afternoon task"
    When the agent logs a new entry for that same day
    Then Log.md has exactly one "## 2026-W09-1 (Mon)" section
    And it contains all three entries in chronological order

  Scenario: Writing logs to a different day still heals duplicates elsewhere
    Given Log.md has two "## 2026-W09-1 (Mon)" sections with different entries
    When the agent writes entries for Tuesday
    Then the duplicate Monday sections are merged into one
    And Tuesday's entries appear in their own section

  # --- Validation ---

  Scenario: Invalid ISO week date is rejected
    When the agent calls WriteLogs with iso_week_date "invalid"
    Then the tool returns an error about the date format

  Scenario: Invalid time format is rejected
    When the agent calls WriteLogs with a time of "25:00"
    Then the tool returns an error about the time format
