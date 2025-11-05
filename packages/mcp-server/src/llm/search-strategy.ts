/**
 * Build prompt for LLM search decision-making
 *
 * @param query - User's search query
 * @param visualization - ASCII graph visualization showing exploration frontier
 * @param iteration - Current iteration number (0-indexed)
 * @param maxIterations - Maximum iterations before forcing stop (default: 10)
 * @returns Formatted prompt string for LLM
 */
export function buildSearchPrompt(
  query: string,
  visualization: string,
  iteration: number,
  maxIterations: number = 10
): string {
  const systemPrompt =
    "You are a search strategist exploring a knowledge graph represented as an Obsidian vault. " +
    "Your task is to decide which notes to explore next based on relevance to the user's query. " +
    "You will be shown a tree visualization of the current exploration frontier.";

  const userPrompt =
    `Query: "${query}"\n\n` +
    `Iteration: ${iteration + 1}/${maxIterations}\n\n` +
    `Current exploration state:\n${visualization}\n\n` +
    "**Your task in two steps:**\n\n" +
    "**Step 1: Evaluate ALL visible notes**\n" +
    "For EVERY note shown in the visualization above, assign a confidence score (0-1) based on:\n" +
    "- **Relevance**: How likely is this note to contain information related to the query? (judge by note name)\n" +
    "- **Connection patterns**: Notes with many connections may be central concepts\n" +
    "- Lower scores (0.1-0.3) for clearly irrelevant notes\n" +
    "- Medium scores (0.4-0.6) for possibly relevant notes\n" +
    "- High scores (0.7-1.0) for highly relevant notes\n\n" +
    "**Step 2: Choose nodes to explore**\n" +
    "Select 0-3 of the most promising notes to explore next. Consider:\n" +
    "- Prioritize notes with highest confidence scores\n" +
    "- Balance depth (following promising paths) vs breadth (checking multiple areas)\n" +
    "- Stop if you've found highly relevant notes OR further exploration seems unlikely to help\n\n" +
    "**First, think through your decision:**\n" +
    "- Evaluate each visible note's relevance to the query\n" +
    "- Which notes have the highest confidence scores?\n" +
    "- Which should be explored and why?\n" +
    "- Should we stop searching or continue?\n\n" +
    "**Then respond with ONLY a raw JSON object (no code fences, no markdown):**\n" +
    "IMPORTANT: Output the JSON object directly. Do NOT wrap it in ```json or ``` code fences.\n\n" +
    "Format:\n" +
    `{\n` +
    `  "nodesToExplore": ["NoteName1", "NoteName2"],\n` +
    `  "shouldStop": false,\n` +
    `  "confidenceScores": {\n` +
    `    "NoteName1": 0.9,\n` +
    `    "NoteName2": 0.7,\n` +
    `    "IrrelevantNote": 0.2\n` +
    `  }\n` +
    `}`;

  return `${systemPrompt}\n\n${userPrompt}`;
}
