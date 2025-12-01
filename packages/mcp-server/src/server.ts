import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListPromptsRequestSchema,
  GetPromptRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { z, ZodRawShape } from "zod";
import { logger } from "./utils/logger.js";
import { parseWikiLinks } from "@webdesserts/obsidian-memory-utils";

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
    annotations?: {
      title?: string;
      readOnlyHint?: boolean;
      destructiveHint?: boolean;
      idempotentHint?: boolean;
      openWorldHint?: boolean;
    };
  }> = [];

  private prompts: Array<{
    name: string;
    title?: string;
    description?: string;
    arguments?: Array<{
      name: string;
      description?: string;
      required?: boolean;
    }>;
  }> = [];

  private handlers = new Map<string, (args: unknown) => Promise<any>>();
  private promptHandlers = new Map<
    string,
    (args: Record<string, string>) => Promise<{
      messages: Array<{ role: string; content: { type: string; text: string } }>;
    }>
  >();

  constructor(config: { name: string; version: string }) {
    this.server = new Server(
      { name: config.name, version: config.version },
      {
        capabilities: {
          tools: {},
          prompts: {},
          roots: {
            listChanged: false,
          },
          resources: {
            listChanged: false,
          },
        },
      }
    );

    this.setupHandlers();
  }

  /**
   * Filter tool call params to only include non-default values
   */
  private filterParams(name: string, args: Record<string, any>): Record<string, any> {
    const filtered: Record<string, any> = {};

    // Define default values for each tool
    const defaults: Record<string, Record<string, any>> = {
      Search: {
        includePrivate: false,
        topK: 10,
        minSimilarity: 0.3,
        debug: false,
      },
      Reflect: {
        includePrivate: false,
      },
    };

    const toolDefaults = defaults[name] || {};

    // Include params that differ from defaults
    for (const [key, value] of Object.entries(args)) {
      if (toolDefaults[key] === undefined || toolDefaults[key] !== value) {
        filtered[key] = value;
      }
    }

    // Special handling for Search tool
    if (name === "Search" && args.query) {
      const wikiLinks = parseWikiLinks(args.query);
      if (wikiLinks.length > 0) {
        filtered.wikiLinks = wikiLinks.map(link => link.target);
      }
    }

    return filtered;
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

        // Log tool call with filtered params
        const params = this.filterParams(name, args);
        if (Object.keys(params).length > 0) {
          logger.info({ tool: name, params }, "Tool called");
        } else {
          logger.info({ tool: name }, "Tool called");
        }

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

    // List available prompts
    this.server.setRequestHandler(ListPromptsRequestSchema, async () => {
      return { prompts: this.prompts };
    });

    // Get prompt
    this.server.setRequestHandler(GetPromptRequestSchema, async (request) => {
      const { name, arguments: args = {} } = request.params;

      try {
        const handler = this.promptHandlers.get(name);
        if (!handler) throw new Error(`Prompt not found: ${name}`);

        const result = await handler(args as Record<string, string>);
        return result;
      } catch (error) {
        const errorMessage =
          error instanceof Error ? error.message : String(error);
        throw new Error(`Error getting prompt: ${errorMessage}`);
      }
    });
  }

  registerTool<TInput extends ZodRawShape>(
    name: string,
    config: {
      title?: string;
      description: string;
      inputSchema: TInput;
      annotations?: {
        title?: string;
        readOnlyHint?: boolean;
        destructiveHint?: boolean;
        idempotentHint?: boolean;
        openWorldHint?: boolean;
      };
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
      ...(config.annotations && { annotations: config.annotations }),
    });

    // Store handler with validation wrapper
    this.handlers.set(name, async (args: unknown) => {
      const validated = z.object(config.inputSchema).parse(args);
      return handler(validated);
    });
  }

  registerPrompt<TArgs extends ZodRawShape>(
    name: string,
    config: {
      title?: string;
      description?: string;
      argsSchema?: TArgs;
    },
    handler: (args: z.infer<z.ZodObject<TArgs>>) => Promise<{
      messages: Array<{ role: string; content: { type: string; text: string } }>;
    }>
  ) {
    // Convert Zod schema to arguments array if provided
    const promptArguments = config.argsSchema
      ? Object.entries(config.argsSchema).map(([argName, schema]) => ({
          name: argName,
          description: (schema as any)._def?.description,
          required: !(schema as any).isOptional(),
        }))
      : undefined;

    // Store prompt definition
    this.prompts.push({
      name,
      title: config.title,
      description: config.description,
      arguments: promptArguments,
    });

    // Store handler with validation wrapper
    this.promptHandlers.set(name, async (args: Record<string, string>) => {
      if (config.argsSchema) {
        const validated = z.object(config.argsSchema).parse(args);
        return handler(validated);
      } else {
        return handler({} as z.infer<z.ZodObject<TArgs>>);
      }
    });
  }
}
