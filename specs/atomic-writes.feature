Feature: Atomic File Writes
  Sync data files should not be corrupted by crashes during writes.

  Scenario: Writing a file completes fully
    Given a sync file "example.loro" exists with content A
    When I write content B to "example.loro"
    Then "example.loro" should contain exactly content B
    And no temporary debris should remain

  Scenario: Crash during a write preserves the previous version
    Given a sync file "example.loro" exists with content A
    When a write crashes before completing
    Then "example.loro" should still contain content A

  Scenario: Creating a new file
    Given "example.loro" does not exist
    When I write content A to "example.loro"
    Then "example.loro" should contain exactly content A
    And no temporary debris should remain
