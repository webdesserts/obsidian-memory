import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * GetCurrentDatetime Tool
 *
 * Get the current date and time in ISO format for timeline entries.
 */
export function registerGetCurrentDatetime(server: McpServer, context: ToolContext) {
  server.registerTool(
    "GetCurrentDatetime",
    {
      title: "Get Current Datetime",
      description:
        "Get the current date and time in ISO format for use in Working Memory timeline entries. Returns ISO 8601 formatted datetime (YYYY-MM-DDTHH:MM) and additional context.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const now = new Date();

      // Format datetime as YYYY-MM-DDTHH:MM
      const year = now.getFullYear();
      const month = String(now.getMonth() + 1).padStart(2, "0");
      const day = String(now.getDate()).padStart(2, "0");
      const hours = String(now.getHours()).padStart(2, "0");
      const minutes = String(now.getMinutes()).padStart(2, "0");

      const isoDatetime = `${year}-${month}-${day}T${hours}:${minutes}`;

      // Get day of week
      const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
      const dayOfWeek = days[now.getDay()];

      return {
        content: [
          {
            type: "text",
            text: `Current datetime: ${isoDatetime}\nDay of week: ${dayOfWeek}\n\nUse this timestamp when creating timeline entries in Working Memory:\n\`\`\`markdown\n## ${isoDatetime} - Session Summary\n- Your timeline entries here...\n\`\`\``,
          },
        ],
        structuredContent: {
          isoDatetime,
          dayOfWeek,
          year,
          month: parseInt(month),
          day: parseInt(day),
          hours: parseInt(hours),
          minutes: parseInt(minutes),
        },
      };
    }
  );
}
