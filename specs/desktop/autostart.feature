Feature: Auto-launch at login

  Scenario: User enables auto-launch from the tray
    Given the desktop app is running
    When the user selects "Launch at Login" from the tray menu
    Then the app is registered to launch at login
    And the "Launch at Login" menu item is checked

  Scenario: User disables auto-launch from the tray
    Given the desktop app is registered to launch at login
    When the user selects "Launch at Login" from the tray menu
    Then the app is removed from login items
    And the "Launch at Login" menu item is unchecked

  Scenario: App reflects autostart state on launch
    Given the app is registered to launch at login
    When the app starts
    Then the "Launch at Login" menu item is checked
