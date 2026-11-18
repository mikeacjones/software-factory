<script lang="ts">
  import { SvelteFlow, Background, Controls, MiniMap, type Edge, type Node } from '@xyflow/svelte';
  import TierNode from './nodes/TierNode.svelte';
  import BeadNode from './nodes/BeadNode.svelte';
  import { projectGraph, type Ref } from '../lib/graph';
  import type { Item, Project } from '../lib/types';

  let {
    project,
    inbox,
    chosen,
    now,
    onchoose,
    map = true,
  }: { project: Project; inbox: Item[]; chosen?: string; now: number; onchoose: (id: string, ref: Ref) => void; map?: boolean } = $props();

  const nodeTypes = { tier: TierNode, bead: BeadNode };
  let nodes = $state.raw<Node[]>([]);
  let edges = $state.raw<Edge[]>([]);
  let refs = new Map<string, Ref>();
  // The clock only moves the words in the nodes; read it once a minute.
  const minute = $derived(Math.floor(now / 60) * 60);

  $effect(() => {
    const built = projectGraph(project, inbox, chosen, minute);
    nodes = built.nodes;
    edges = built.edges;
    refs = built.refs;
  });
</script>

<div class="map">
  <SvelteFlow
    bind:nodes
    bind:edges
    {nodeTypes}
    fitView
    fitViewOptions={{ padding: 0.12, maxZoom: 1 }}
    minZoom={0.15}
    maxZoom={1.6}
    nodesDraggable={false}
    nodesConnectable={false}
    elementsSelectable={true}
    onnodeclick={({ node }) => {
      const ref = refs.get(node.id);
      if (ref) onchoose(node.id, ref);
    }}
    proOptions={{ hideAttribution: true }}
  >
    <Background gap={20} />
    <Controls showLock={false} />
    {#if map}<MiniMap pannable zoomable nodeStrokeWidth={2} />{/if}
  </SvelteFlow>
</div>

<style>
  .map { width: 100%; height: 100%; }
</style>
