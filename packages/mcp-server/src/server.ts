import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { z, ZodRawShape } from "zod";

/**
 * Wrapper around SDK Server that mimics McpServer.registerTool() API
 * Uses Zod v4 schemas instead of v3
 *
 * When SDK supports Zod v4, replace with:
 * import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
 */
export class McpServer {
  public readonly server: Server;

  private tools: Array<{
    name: string;
    title?: string;
    description: string;
    inputSchema: any;
  }> = [];

  private handlers = new Map<string, (args: unknown) => Promise<any>>();

  constructor(config: { name: string; version: string }) {
    this.server = new Server(
      { name: config.name, version: config.version },
      {
        capabilities: {
          tools: {},
          roots: {
            listChanged: false,
          },
        },
      }
    );

    this.setupHandlers();
  }

  private setupHandlers() {
    // List available tools
    this.server.setRequestHandler(ListToolsRequestSchema, async () => {
      return { tools: this.tools };
    });

    // Handle tool calls
    this.server.setRequestHandler(CallToolRequestSchema, async (request) => {
      const { name, arguments: args = {} } = request.params;

      try {
        const handler = this.handlers.get(name);
        if (!handler) throw new Error(`Tool not found: ${name}`);

        const result = await handler(args);

        return {
          content: result.content,
          ...(result.isError !== undefined && { isError: result.isError }),
        };
      } catch (error) {
        const errorMessage =
          error instanceof Error ? error.message : String(error);
        return {
          content: [{ type: "text", text: `Error: ${errorMessage}` }],
          isError: true,
        };
      }
    });
  }

  registerTool<TInput extends ZodRawShape>(
    name: string,
    config: {
      title?: string;
      description: string;
      inputSchema: TInput;
    },
    handler: (args: z.infer<z.ZodObject<TInput>>) => Promise<{
      content: Array<{ type: string; text?: string; [key: string]: any }>;
      structuredContent?: Record<string, unknown>;
      isError?: boolean;
    }>
  ) {
    // Store tool definition
    this.tools.push({
      name,
      title: config.title,
      description: config.description,
      inputSchema: z.toJSONSchema(z.object(config.inputSchema)),
    });

    // Store handler with validation wrapper
    this.handlers.set(name, async (args: unknown) => {
      const validated = z.object(config.inputSchema).parse(args);
      return handler(validated);
    });
  }
}
