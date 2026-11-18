<script lang="ts">
  // A bead on a project's map: its title, where its pipeline stands, and
  // the pipeline's steps as a row of marks.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';
  import type { Tone } from '../../lib/format';
  import type { Step } from '../../lib/types';

  type Data = {
    id: string;
    title: string;
    tone: Tone;
    word: string;
    where: string;
    steps: Step[];
    here?: string;
    going: boolean;
    over: boolean;
    ship: boolean;
    asks: boolean;
    chosen: boolean;
  };
  let { data }: NodeProps & { data: Data } = $props();

  function mark(step: Step): string {
    if (step.id === data.here && !data.over) return data.going ? 'now' : 'stopped';
    return step.rounds ? (step.rounds > 1 ? 'done again' : 'done') : '';
  }
</script>

<Handle type="target" position={Position.Left} />
<div class="bead" class:ship={data.ship} class:going={data.going && data.tone === 'running'} class:asks={data.asks} class:chosen={data.chosen}>
  <div class="top">
    <span class="id mono">{data.ship ? 'ship' : data.id}</span>
    <span class="pill {data.tone}">{data.word}</span>
  </div>
  <p class="title" title={data.title}>{data.title}</p>
  {#if data.steps.length}
    <div class="track" aria-label="The pipeline's steps">
      {#each data.steps as step (step.id)}<i class="st {mark(step)}" title="{step.id}{step.rounds > 1 ? `, ${step.rounds} times` : ''}"></i>{/each}
    </div>
  {/if}
  {#if data.where}<p class="where">{data.where}</p>{/if}
</div>
<Handle type="source" position={Position.Right} />

<style>
  .bead {
    width: 250px; padding: 8px 10px; border-radius: var(--radius); background: var(--surface);
    border: 1px solid var(--line); display: flex; flex-direction: column; gap: 4px; cursor: pointer;
  }
  .bead:hover { border-color: var(--ink-3); }
  .bead.ship { border-style: dashed; border-color: var(--line-2); background: var(--sunk); }
  .bead.going { border-color: color-mix(in srgb, var(--accent) 60%, var(--line)); }
  .bead.asks { border-color: var(--warning-solid); box-shadow: 0 0 0 3px var(--warning-soft); }
  .bead.chosen { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  .top { display: flex; justify-content: space-between; align-items: center; gap: 6px; }
  .id { font-size: 11px; color: var(--ink-3); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .title { font-size: 12px; font-weight: 550; line-height: 1.3; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .where { font-size: 11px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .track { display: flex; gap: 2px; }
  .st { flex: 1; height: 5px; border-radius: 2px; background: var(--line); }
  .st.done { background: var(--good); }
  .st.again { background: color-mix(in srgb, var(--good) 60%, var(--warning-solid)); }
  .st.now { background: var(--accent); animation: beat 1.4s ease-in-out infinite; }
  .st.stopped { background: var(--warning-solid); }
</style>
