import {
  getLlama,
  type Llama,
  type LlamaModel,
  type LlamaContext,
  LlamaChatSession,
} from "node-llama-cpp";
import path from "path";
import { fileURLToPath } from "url";
import { buildSearchPrompt } from "./search-strategy.js";
import type { SearchDecision } from "./types.js";
import { debugLog } from "../utils/logger.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Singleton manager for local LLM inference
 *
 * Handles lazy initialization, model loading, and search decision generation.
 * Loads DeepSeek-R1-Distill-Qwen-14B for reasoning-based graph exploration.
 */
export class LLMManager {
  private static instance: LLMManager | null = null;

  private llama?: Llama;
  private model?: LlamaModel;
  private context?: LlamaContext;

  private constructor() {}

  /**
   * Get singleton instance, initializing on first call
   */
  static async getInstance(): Promise<LLMManager> {
    if (!LLMManager.instance) {
      LLMManager.instance = new LLMManager();
      await LLMManager.instance.initialize();
    }
    return LLMManager.instance;
  }

  /**
   * Initialize LLM: load model only (contexts created per-search)
   */
  private async initialize(): Promise<void> {
    const modelPath = path.join(
      __dirname,
      "../../models",
      "DeepSeek-R1-Distill-Qwen-14B-Q4_K_M.gguf"
    );

    debugLog("[LLM] Initializing model...");

    try {
      this.llama = await getLlama();
      this.model = await this.llama.loadModel({ modelPath });

      debugLog("[LLM] Model ready");
    } catch (error) {
      debugLog(`[LLM] Initialization error: ${error}`);
      if (error instanceof Error && error.message.includes("ENOENT")) {
        throw new Error(
          `Model not found at: ${modelPath}\n\n` +
            `Please download the model:\n` +
            `  cd packages/mcp-server\n` +
            `  npm run download-model\n\n` +
            `See models/README.md for more details.`
        );
      }
      throw error;
    }
  }

  /**
   * Check if LLM is initialized and ready
   */
  isInitialized(): boolean {
    return !!(this.llama && this.model);
  }

  /**
   * Generate search decision using local LLM
   *
   * Uses LlamaCompletion for single-shot generation without conversation history.
   * Creates fresh context at iteration 0, reuses it for subsequent iterations.
   *
   * @param query - User's search query
   * @param visualization - ASCII graph visualization
   * @param iteration - Current iteration number
   * @param maxIterations - Maximum iterations allowed
   * @returns Search decision with nodes to explore and confidence scores
   */
  async generateSearchDecision(
    query: string,
    visualization: string,
    iteration: number,
    maxIterations: number = 10
  ): Promise<SearchDecision> {
    debugLog(`[LLM] generateSearchDecision called (iteration ${iteration})`);

    if (!this.isInitialized()) {
      throw new Error("LLM not initialized");
    }

    // Create fresh context at start of new search
    if (iteration === 0) {
      if (this.context) {
        await this.context.dispose();
      }
      this.context = await this.model!.createContext();
      debugLog("[Search] Starting new search (iteration 0)");
    }

    const prompt = buildSearchPrompt(query, visualization, iteration, maxIterations);

    // Create fresh chat session for this iteration to avoid context buildup
    const session = new LlamaChatSession({
      contextSequence: this.context!.getSequence(),
    });

    const response = await session.prompt(prompt, {
      maxTokens: 1000,
      temperature: 0.7,
    });

    let parsed;
    try {
      // Extract JSON from response (might have extra text)
      const jsonMatch = response.match(/\{[\s\S]*\}/);
      if (!jsonMatch) {
        throw new Error("No JSON object found in response");
      }

      // Extract thinking (everything before the JSON)
      const thinkingText = response.substring(0, jsonMatch.index).trim();
      if (thinkingText) {
        debugLog(`[LLM] Iteration ${iteration} thinking:\n${thinkingText}`);
      }

      parsed = JSON.parse(jsonMatch[0]);
    } catch (error) {
      debugLog(`[LLM] JSON parse failed: ${error}`);
      debugLog(`[LLM] Full response: ${response}`);
      // Return safe default on parse failure
      return {
        nodesToExplore: [],
        shouldStop: true,
        confidenceScores: new Map(),
      };
    }

    const nodeCount = parsed.nodesToExplore?.length || 0;
    const nodeList = nodeCount > 0 ? parsed.nodesToExplore.join(", ") : "none";
    debugLog(`[Search] Iteration ${iteration} decision: explore ${nodeCount} nodes [${nodeList}], stop=${parsed.shouldStop}`);

    return {
      nodesToExplore: parsed.nodesToExplore || [],
      shouldStop: parsed.shouldStop ?? true,
      confidenceScores: new Map(Object.entries(parsed.confidenceScores || {})),
    };
  }

  /**
   * Cleanup resources (model, context)
   */
  async dispose(): Promise<void> {
    if (this.context) {
      await this.context.dispose();
    }
    this.context = undefined;
    this.model = undefined;
    this.llama = undefined;
    debugLog("[LLM] Disposed");
  }
}
