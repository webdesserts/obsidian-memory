/**
 * Decision returned by LLM during graph search exploration
 */
export interface SearchDecision {
  /** Note names to explore in the next iteration */
  nodesToExplore: string[];

  /** Whether to stop exploration (query satisfied or max depth reached) */
  shouldStop: boolean;

  /** Confidence scores for each discovered note (0-1 scale) */
  confidenceScores: Map<string, number>;
}

/**
 * Options for building search prompts
 */
export interface SearchPromptOptions {
  /** The user's search query */
  query: string;

  /** ASCII visualization of current graph state */
  visualization: string;

  /** Current iteration number (0-indexed) */
  iteration: number;

  /** Maximum iterations allowed before forcing stop */
  maxIterations: number;
}
