<script lang="ts">
  import { untrack } from 'svelte';
  // A project's map. It is laid out top-down by itself; whatever the owner
  // drags stays where it was put (in this browser) until Tidy.
  import { SvelteFlow, Background, Controls, MiniMap, type Edge, type Node } from '@xyflow/svelte';
  import TierNode from './nodes/TierNode.svelte';
  import BeadNode from './nodes/BeadNode.svelte';
  import StepNode from './nodes/StepNode.svelte';
  import PipelineNode from './nodes/PipelineNode.svelte';
  import ChipNode from './nodes/ChipNode.svelte';
  import AgentNode from './nodes/AgentNode.svelte';
  import MapTools from './MapTools.svelte';
  import { projectGraph, type Pipelines, type Ref } from '../lib/graph';
  import { load, save, loadChoice, saveChoice, loadOpen, saveOpen, type Placed } from '../lib/arrangement';
  import { keep } from '../lib/same';
  import type { Item, Project } from '../lib/types';

  let {
    project,
    inbox,
    chosen,
    now,
    onchoose,
    map = true,
  }: { project: Project; inbox: Item[]; chosen?: string; now: number; onchoose: (id: string, ref: Ref) => void; map?: boolean } = $props();

  const nodeTypes = { tier: TierNode, bead: BeadNode, step: StepNode, pipeline: PipelineNode, chip: ChipNode, agent: AgentNode };
  let nodes = $state.raw<Node[]>([]);
  let edges = $state.raw<Edge[]>([]);
  let refs = new Map<string, Ref>();
  // The map is made afresh for each project (ProjectView keys it by one).
  // svelte-ignore state_referenced_locally
  let placed: Placed = $state(load(project.id));
  // svelte-ignore state_referenced_locally
  let open: Record<string, boolean> = $state(loadOpen(project.id));
  let pipelines: Pipelines = $state(loadChoice('pipelines', ['active', 'all', 'cards'] as const, 'active'));
  let dragging = $state(false);
  // The clock only moves the words in the nodes; read it once a minute.
  const minute = $derived(Math.floor(now / 60) * 60);

  $effect(() => saveChoice('pipelines', pipelines));

  $effect(() => {
    const built = projectGraph(project, inbox, chosen, minute, { pipelines, open, ontoggle: toggle });
    if (dragging) return;
    // A card the owner placed keeps its place; the rest are laid out.
    // What is drawn already and has not changed is kept as it is.
    const fresh = built.nodes.map((node) => (placed[node.id] ? { ...node, position: placed[node.id] } : node));
    const nextNodes = keep(untrack(() => nodes), fresh);
    const nextEdges = keep(untrack(() => edges), built.edges);
    if (nextNodes !== untrack(() => nodes)) nodes = nextNodes;
    if (nextEdges !== untrack(() => edges)) edges = nextEdges;
    refs = built.refs;
  });

  function stopped({ nodes: moved }: { nodes: Node[] }) {
    const next = { ...placed };
    for (const node of moved) next[node.id] = { x: Math.round(node.position.x), y: Math.round(node.position.y) };
    placed = next;
    save(project.id, next);
    dragging = false;
  }

  /** Open or fold one card. A card that changes size is laid out again
   * where the owner had not placed it. */
  function toggle(id: string, value: boolean) {
    open = { ...open, [id]: value };
    saveOpen(project.id, open);
  }

  // A new default for every card forgets the choices made for single ones.
  function choose(mode: Pipelines) {
    open = {};
    saveOpen(project.id, open);
    pipelines = mode;
  }

  function tidy() {
    placed = {};
    save(project.id, {});
  }
</script>

<div class="map">
  <SvelteFlow
    bind:nodes
    bind:edges
    {nodeTypes}
    fitView
    fitViewOptions={{ padding: 0.1, maxZoom: 1 }}
    minZoom={0.08}
    maxZoom={1.6}
    nodesDraggable={true}
    nodesConnectable={false}
    elementsSelectable={true}
    nodeDragThreshold={4}
    onnodedragstart={() => (dragging = true)}
    onnodedragstop={stopped}
    onnodeclick={({ node }) => {
      const ref = refs.get(node.id);
      if (ref) onchoose(node.id.split('::')[0], ref);
    }}
    proOptions={{ hideAttribution: true }}
  >
    <Background gap={20} />
    <Controls showLock={false} />
    {#if map}<MiniMap pannable zoomable nodeStrokeWidth={2} />{/if}
    <MapTools {pipelines} onchoose={choose} arranged={Object.keys(placed).length} ontidy={tidy} />
  </SvelteFlow>
</div>

<style>
  .map { width: 100%; height: 100%; }
  .map :global(.svelte-flow__node) { cursor: grab; }
  .map :global(.svelte-flow__node.dragging) { cursor: grabbing; }
</style>
