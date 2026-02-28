Feature: Note operations
  MCP tools for managing notes in the vault — creating, reading, deleting,
  and moving. All tools accept flexible note references: wiki-links
  (`[[Note]]`), memory URIs (`memory:path/Note`), or plain names.

  Background:
    Given a vault with a graph index

  # --- Note resolution ---

  Scenario: Plain name resolves a note in a subdirectory
    Given "My Note" exists at knowledge/My Note.md
    When the agent deletes "My Note"
    Then knowledge/My Note.md is removed

  Scenario: Wiki-link resolves a note in a subdirectory
    Given "My Note" exists at knowledge/My Note.md
    When the agent deletes "[[My Note]]"
    Then knowledge/My Note.md is removed

  Scenario: Memory URI resolves directly
    Given "My Note" exists at knowledge/My Note.md
    When the agent deletes "memory:knowledge/My Note"
    Then knowledge/My Note.md is removed

  Scenario: Deleting a nonexistent note returns an error
    When the agent deletes "Ghost Note"
    Then the tool returns an error about the note not being found

  # --- Move / rename ---

  Scenario: Moving a note by plain name from a subdirectory
    Given "My Note" exists at knowledge/My Note.md
    When the agent moves "My Note" to "archive/My Note"
    Then knowledge/My Note.md no longer exists
    And archive/My Note.md contains the original content

  Scenario: Moving updates backlinks in other notes
    Given "Target" exists and "Linker" contains [[Target]]
    When the agent moves "Target" to "Renamed"
    Then "Linker" now contains [[Renamed]]

  Scenario: Move destination uses literal path (not graph lookup)
    Given "Note" exists at the root
    When the agent moves "Note" to "knowledge/Note"
    Then the note lives at knowledge/Note.md
    And no graph lookup is performed for the destination
