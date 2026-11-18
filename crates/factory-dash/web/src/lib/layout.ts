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
  // An edge may weigh more than another: dagre keeps heavy edges short
  // and straight, which holds a pipeline's main path in one line.
  for (const edge of edges) {
    const weight = Number((edge.data as { weight?: number } | undefined)?.weight ?? 1);
    graph.setEdge(edge.source, edge.target, { weight, minlen: 1 });
  }
  dagre.layout(graph);
  const across = (options.rankdir ?? 'LR') === 'LR';
  // dagre centres each node on its rank. Cards of different sizes in one
  // rank read better lined up along the side the edges come in from.
  const reach = new Map<number, number>();
  for (const node of nodes) {
    const at = graph.node(node.id);
    const centre = Math.round(across ? at.x : at.y);
    reach.set(centre, Math.max(reach.get(centre) ?? 0, across ? node.width : node.height));
  }
  return nodes.map((node) => {
    const at = { ...graph.node(node.id) };
    const centre = Math.round(across ? at.x : at.y);
    const shift = ((reach.get(centre) ?? 0) - (across ? node.width : node.height)) / 2;
    if (across) at.x -= shift;
    else at.y -= shift;
    return {
      ...node,
      targetPosition: across ? Position.Left : Position.Top,
      sourcePosition: across ? Position.Right : Position.Bottom,
      position: { x: at.x - node.width / 2, y: at.y - node.height / 2 },
    };
  });
}
