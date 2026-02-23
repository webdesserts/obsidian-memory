Feature: Daemon Lock
  Only one sync daemon may run on a given vault at a time.

  Scenario: Starting the first daemon
    Given no daemon is running on the vault
    When I start a daemon
    Then the daemon should start successfully

  Scenario: Starting a second daemon on the same vault
    Given a daemon is running on the vault
    When I start another daemon on the same vault
    Then it should exit with a clear error message
    And the first daemon should continue running unaffected

  Scenario: Restarting after a clean shutdown
    Given a daemon was running on the vault
    When the daemon shuts down cleanly
    And I start a new daemon
    Then the new daemon should start successfully

  Scenario: Restarting after a crash
    Given a daemon was running on the vault
    When the daemon process crashes or is killed
    And I start a new daemon
    Then the new daemon should start successfully
    And no manual cleanup should be required
