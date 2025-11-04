/**
 * Graph visualization utilities for rendering ASCII tree structures
 * from knowledge graph neighborhoods
 */

/**
 * Neighborhood data structure from GraphIndex
 */
export interface NeighborhoodNode {
  distance: number;
  linkType: "forward" | "backward" | "both";
  directLinks: string[];
  backlinks: string[];
}

/**
 * Internal tree node structure for rendering
 */
interface TreeNode {
  name: string;
  path?: string; // Only shown if duplicate note names exist
  arrow: "→" | "←";
  unexploredCount?: number; // Only for leaf nodes
  children: TreeNode[];
}

/**
 * Render a graph neighborhood as an ASCII tree structure
 *
 * @param startNode - The starting node name
 * @param neighborhood - Map of note names to their neighborhood data
 * @param duplicateNotes - Set of note names that have duplicates (need path annotation)
 * @param getPathForNote - Function to get the path for a note name
 * @returns ASCII tree visualization string
 *
 * @example
 * ```
 * Exploring from: Anchor Step
 *
 * ├─→ West Coast Swing
 * │   ├─→ Compression [3 unexplored]
 * │   ├─→ Connection [4 unexplored]
 * │   └─← Stretch [3 unexplored]
 * │
 * └─→ Pattern
 *     └─→ Pass [4 unexplored]
 * ```
 */
export function renderGraphTree(
  startNode: string,
  neighborhood: Map<string, NeighborhoodNode>,
  duplicateNotes: Set<string>,
  getPathForNote: (name: string) => string | undefined
): string {
  // Separate nodes by distance
  const distance1Nodes: string[] = [];
  const distance2Nodes: string[] = [];

  for (const [noteName, data] of neighborhood.entries()) {
    if (data.distance === 1) {
      distance1Nodes.push(noteName);
    } else if (data.distance === 2) {
      distance2Nodes.push(noteName);
    }
  }

  // Sort to ensure stable ordering for grouping algorithm
  distance1Nodes.sort();
  distance2Nodes.sort();

  // Group distance-2 nodes under distance-1 parents
  const parentChildMap = groupNodesByParent(
    distance2Nodes,
    distance1Nodes,
    neighborhood
  );

  // Build tree nodes for distance-1 (expanded nodes)
  const treeNodes: TreeNode[] = [];

  for (const parentName of distance1Nodes) {
    const parentData = neighborhood.get(parentName)!;
    const children = parentChildMap.get(parentName) || [];

    // Build child tree nodes
    const childNodes: TreeNode[] = children.map((childName) => {
      const childData = neighborhood.get(childName)!;
      return {
        name: childName,
        path: shouldShowPath(childName, duplicateNotes)
          ? getPathForNote(childName)
          : undefined,
        arrow: determineArrow(childData.linkType, parentName, childData),
        unexploredCount: calculateUnexploredCount(childData),
        children: [],
      };
    });

    // Sort children: forward links first, backlinks last
    childNodes.sort((a, b) => {
      if (a.arrow === "→" && b.arrow === "←") return -1;
      if (a.arrow === "←" && b.arrow === "→") return 1;
      return a.name.localeCompare(b.name);
    });

    treeNodes.push({
      name: parentName,
      path: shouldShowPath(parentName, duplicateNotes)
        ? getPathForNote(parentName)
        : undefined,
      arrow: determineArrow(parentData.linkType, startNode, parentData),
      children: childNodes,
    });
  }

  // Sort top-level nodes: forward links first, backlinks last
  treeNodes.sort((a, b) => {
    if (a.arrow === "→" && b.arrow === "←") return -1;
    if (a.arrow === "←" && b.arrow === "→") return 1;
    return a.name.localeCompare(b.name);
  });

  // Render the tree
  let output = `Exploring from: ${startNode}\n\n`;
  output += renderTreeNodes(treeNodes, "");

  return output;
}

/**
 * Group distance-2 nodes under their strongest parent based on connection strength
 */
function groupNodesByParent(
  distance2Nodes: string[],
  distance1Nodes: string[],
  neighborhood: Map<string, NeighborhoodNode>
): Map<string, string[]> {
  const parentChildMap = new Map<string, string[]>();

  // Initialize empty arrays for each parent
  for (const parent of distance1Nodes) {
    parentChildMap.set(parent, []);
  }

  // For each distance-2 node, find its strongest parent
  for (const childName of distance2Nodes) {
    const childData = neighborhood.get(childName)!;

    // Find which distance-1 nodes this child connects to
    const potentialParents = distance1Nodes.filter((parent) => {
      const parentData = neighborhood.get(parent)!;
      return (
        parentData.directLinks.includes(childName) ||
        parentData.backlinks.includes(childName)
      );
    });

    if (potentialParents.length === 0) {
      // Orphan node - skip (shouldn't happen in a proper neighborhood)
      continue;
    }

    if (potentialParents.length === 1) {
      // Single parent - easy case
      parentChildMap.get(potentialParents[0])!.push(childName);
      continue;
    }

    // Multiple parents - calculate connection strength
    let bestParent = potentialParents[0];
    let bestStrength = 0;

    for (const parent of potentialParents) {
      const strength = calculateConnectionStrength(
        childName,
        parent,
        distance2Nodes,
        neighborhood
      );

      if (strength > bestStrength) {
        bestStrength = strength;
        bestParent = parent;
      }
    }

    parentChildMap.get(bestParent)!.push(childName);
  }

  return parentChildMap;
}

/**
 * Calculate connection strength between a child node and a potential parent
 * Strength = direct links + shared sibling connections
 */
function calculateConnectionStrength(
  childName: string,
  parentName: string,
  allDistance2Nodes: string[],
  neighborhood: Map<string, NeighborhoodNode>
): number {
  const childData = neighborhood.get(childName)!;
  const parentData = neighborhood.get(parentName)!;

  let strength = 0;

  // Direct connection to parent (weight: 1)
  const hasDirectLink =
    parentData.directLinks.includes(childName) ||
    parentData.backlinks.includes(childName);
  if (hasDirectLink) {
    strength += 1;
  }

  // Count shared connections with other distance-2 siblings (weight: 0.5 each)
  const siblings = allDistance2Nodes.filter((node) => node !== childName);

  for (const sibling of siblings) {
    const siblingData = neighborhood.get(sibling);
    if (!siblingData) continue;

    // Check if both child and sibling connect to the same parent
    const siblingConnectsToParent =
      parentData.directLinks.includes(sibling) ||
      parentData.backlinks.includes(sibling);

    if (siblingConnectsToParent) {
      // Check if child and sibling are connected to each other
      const childLinksToSibling =
        childData.directLinks.includes(sibling) ||
        childData.backlinks.includes(sibling);

      if (childLinksToSibling) {
        strength += 0.5;
      }
    }
  }

  return strength;
}

/**
 * Determine arrow direction for a node
 * Forward link takes precedence if both forward and backward exist
 */
function determineArrow(
  linkType: "forward" | "backward" | "both",
  parentName: string,
  nodeData: NeighborhoodNode
): "→" | "←" {
  // If it's a 'both' type, forward takes precedence
  if (linkType === "both" || linkType === "forward") {
    return "→";
  }
  return "←";
}

/**
 * Calculate count of unexplored links from this node
 */
function calculateUnexploredCount(nodeData: NeighborhoodNode): number {
  // Total links minus already explored (which would be in the neighborhood)
  // For simplicity, we'll use the total count of direct + backlinks
  // In a real implementation, this would exclude already-visited nodes
  return nodeData.directLinks.length + nodeData.backlinks.length;
}

/**
 * Check if path should be shown for a note (only if duplicates exist)
 */
function shouldShowPath(
  noteName: string,
  duplicateNotes: Set<string>
): boolean {
  return duplicateNotes.has(noteName);
}

/**
 * Recursively render tree nodes with proper ASCII connectors
 */
function renderTreeNodes(nodes: TreeNode[], prefix: string): string {
  let output = "";

  nodes.forEach((node, index) => {
    const isLast = index === nodes.length - 1;
    const connector = isLast ? "└─" : "├─";

    // Build node label
    let label = `${connector}${node.arrow} ${node.name}`;

    if (node.path) {
      label += ` (${node.path})`;
    }

    if (node.unexploredCount !== undefined && node.unexploredCount > 0) {
      label += ` [${node.unexploredCount} unexplored]`;
    }

    output += prefix + label + "\n";

    // Render children
    if (node.children.length > 0) {
      const childPrefix = prefix + (isLast ? "    " : "│   ");
      output += renderTreeNodes(node.children, childPrefix);
    }

    // Add blank line after top-level nodes (except last)
    if (prefix === "" && !isLast) {
      output += "│\n";
    }
  });

  return output;
}
