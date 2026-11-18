<script lang="ts">
  // A step of a bead's pipeline, or where a route out of the flow leads.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';

  type Data = {
    id: string;
    kind: string;
    role?: string;
    model?: string;
    rounds: number;
    state: 'now' | 'stopped' | 'asks' | 'done' | 'todo' | 'end';
    end: boolean;
  };
  let { data, targetPosition = Position.Left, sourcePosition = Position.Right }: NodeProps & { data: Data } = $props();

  const kind: Record<string, string> = { agent: 'agent', command: 'command', commands: 'commands', action: 'action' };
</script>

<Handle type="target" position={targetPosition} />
{#if data.end}
  <div class="end">{data.id}</div>
{:else}
  <div class="step {data.state}">
    <div class="top">
      <span class="name">{data.id}</span>
      {#if data.rounds}<span class="rounds" title="Reached {data.rounds} time{data.rounds === 1 ? '' : 's'}">×{data.rounds}</span>{/if}
    </div>
    <p class="sub">{data.role || kind[data.kind] || data.kind}{data.model ? ` · ${data.model.replace(/^claude-/, '')}` : ''}</p>
  </div>
{/if}
<Handle type="source" position={sourcePosition} />

<style>
  .step {
    width: 140px; padding: 6px 9px; border-radius: 7px; border: 1px solid var(--line-2);
    background: var(--surface); display: flex; flex-direction: column; gap: 1px;
  }
  .step.todo { border-style: dashed; color: var(--ink-3); background: transparent; }
  .step.done { border-color: color-mix(in srgb, var(--good) 50%, var(--line-2)); }
  .step.now { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); animation: glow 1.8s ease-in-out infinite; }
  .step.stopped, .step.asks { border-color: var(--warning-solid); box-shadow: 0 0 0 3px var(--warning-soft); }
  @keyframes glow { 50% { box-shadow: 0 0 0 6px var(--accent-soft); } }
  .top { display: flex; justify-content: space-between; align-items: baseline; gap: 6px; }
  .name { font-weight: 600; font-size: 12px; font-family: var(--mono); }
  .rounds { font-size: 11px; color: var(--good); font-weight: 700; }
  .sub { font-size: 10px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .end {
    padding: 3px 10px; border-radius: 999px; font-size: 11px; font-family: var(--mono);
    background: var(--sunk); color: var(--ink-2); border: 1px solid var(--line);
  }
</style>
