<script lang="ts">
  // A bead's pipeline: its flow's steps, the routes between them, and the
  // way the bead has come, with the step it stands at lit.
  import { SvelteFlow, Background, Controls, type Edge, type Node } from '@xyflow/svelte';
  import StepNode from './nodes/StepNode.svelte';
  import { pipelineGraph } from '../lib/graph';
  import type { Bead } from '../lib/types';

  let { bead, direction = 'TB', height = 520 }: { bead: Bead; direction?: 'LR' | 'TB'; height?: number | string } = $props();
  const nodeTypes = { step: StepNode };
  let nodes = $state.raw<Node[]>([]);
  let edges = $state.raw<Edge[]>([]);

  $effect(() => {
    const built = pipelineGraph(bead, direction);
    nodes = built.nodes;
    edges = built.edges;
  });
</script>

<div class="pipeline" style:height={typeof height === 'number' ? `${height}px` : height}>
  {#if nodes.length}
    <SvelteFlow
      bind:nodes
      bind:edges
      {nodeTypes}
      fitView
      fitViewOptions={{ padding: 0.06, minZoom: direction === 'TB' ? 0.55 : 0.3 }}
      minZoom={0.2}
      maxZoom={1.4}
      nodesDraggable={false}
      nodesConnectable={false}
      elementsSelectable={false}
      zoomOnScroll={false}
      preventScrolling={false}
      proOptions={{ hideAttribution: true }}
    >
      <Background gap={16} />
      <Controls showLock={false} />
    </SvelteFlow>
  {:else}
    <p class="quiet">This bead's pipeline has not started, so its flow is not known yet.</p>
  {/if}
</div>

<style>
  .pipeline { width: 100%; border: 1px solid var(--line); border-radius: var(--radius); overflow: hidden; }
  .pipeline p { padding: 12px; }
</style>
