// Lay a graph out left to right with dagre: Svelte Flow places nodes by
// their top-left corner, dagre by their centre.

import dagre from '@dagrejs/dagre';
import { Position, type Edge, type Node } from '@xyflow/svelte';

export interface Sized {
  width: number;
  height: number;
}

export function layout<N extends Node>(
  nodes: (N & Sized)[],
  edges: Edge[],
  options: { rankdir?: 'LR' | 'TB'; ranksep?: number; nodesep?: number } = {},
): N[] {
  const graph = new dagre.graphlib.Graph();
  graph.setDefaultEdgeLabel(() => ({}));
  graph.setGraph({
    rankdir: options.rankdir ?? 'LR',
    ranksep: options.ranksep ?? 70,
    nodesep: options.nodesep ?? 14,
    marginx: 10,
    marginy: 10,
  });
  for (const node of nodes) graph.setNode(node.id, { width: node.width, height: node.height });
  for (const edge of edges) graph.setEdge(edge.source, edge.target);
  dagre.layout(graph);
  const across = (options.rankdir ?? 'LR') === 'LR';
  return nodes.map((node) => {
    const at = graph.node(node.id);
    return {
      ...node,
      targetPosition: across ? Position.Left : Position.Top,
      sourcePosition: across ? Position.Right : Position.Bottom,
      position: { x: at.x - node.width / 2, y: at.y - node.height / 2 },
    };
  });
}
