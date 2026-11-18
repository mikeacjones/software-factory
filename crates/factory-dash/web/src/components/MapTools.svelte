<script lang="ts">
  // The map's own tools, inside the flow so that it can fit the view.
  import { Panel, useSvelteFlow } from '@xyflow/svelte';
  import type { Pipelines } from '../lib/graph';

  let {
    pipelines,
    onchoose,
    arranged,
    ontidy,
  }: { pipelines: Pipelines; onchoose: (mode: Pipelines) => void; arranged: number; ontidy: () => void } = $props();
  const flow = useSvelteFlow();

  const choices: [Pipelines, string, string][] = [
    ['active', 'Open what is active', 'Open the pipeline of every bead running, parked or asking you, and the agents of every tier at work now'],
    ['all', 'Open all', 'Open every pipeline and every tier\'s agents'],
    ['cards', 'All cards', 'Fold everything to cards'],
  ];

  async function tidy() {
    ontidy();
    // Let the map take its new places before fitting it to the view.
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    flow.fitView({ padding: 0.1, maxZoom: 1, duration: 300 });
  }

  $effect(() => {
    void pipelines;
    requestAnimationFrame(() => requestAnimationFrame(() => flow.fitView({ padding: 0.1, maxZoom: 1, duration: 300 })));
  });
</script>

<Panel position="top-right">
  <div class="tools">
    <div class="seg" role="radiogroup" aria-label="How beads are drawn">
      {#each choices as [value, label, title] (value)}
        <button role="radio" aria-checked={pipelines === value} class:on={pipelines === value} {title} onclick={() => onchoose(value)}>{label}</button>
      {/each}
    </div>
    <button class="btn tidy" onclick={tidy} title="Forget where you moved things and lay the map out again">
      ✦ Tidy{arranged ? ` (${arranged} moved)` : ''}
    </button>
  </div>
</Panel>

<style>
  .tools { display: flex; gap: 8px; align-items: center; }
  .seg { display: flex; background: var(--surface); border: 1px solid var(--line-2); border-radius: 7px; padding: 2px; box-shadow: var(--shadow); }
  .seg button { border: 0; background: transparent; padding: 3px 10px; border-radius: 5px; font-size: 12px; cursor: pointer; color: var(--ink-2); }
  .seg button.on { background: var(--ink); color: var(--plane); }
  .tidy { box-shadow: var(--shadow); }
</style>
