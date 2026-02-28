Feature: MCP server health check
  The MCP HTTP server exposes a health endpoint so container orchestrators
  can verify the server is ready to accept connections.

  Scenario: Health endpoint returns 200
    Given the MCP server is running in HTTP mode
    When a client sends GET /health
    Then the response status is 200
    And the body is "OK"

  Scenario: Docker healthcheck uses the HTTP endpoint
    Given the server is running in a Docker container
    Then the container healthcheck uses curl against /health
    And the container does not depend on procps or pgrep
