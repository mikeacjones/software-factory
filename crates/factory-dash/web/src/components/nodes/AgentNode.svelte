<script lang="ts">
  // One of a tier's agents: how often it ran, on what, and at what cost.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';

  type Data = { label: string; runs: number; failed: number; cost: string; model: string; now: boolean; size: { width: number; height: number } };
  let { data }: NodeProps & { data: Data } = $props();
</script>

<Handle type="target" position={Position.Top} />
<Handle type="target" id="left-in" position={Position.Left} />
<div class="agent" class:now={data.now} style:width="{data.size.width}px" style:height="{data.size.height}px">
  <div class="top"><b>{data.label}</b>{#if data.now}<span class="pill running">at work</span>{/if}</div>
  <p class="facts">
    {data.runs} run{data.runs === 1 ? '' : 's'}{data.failed ? ` · ${data.failed} failed` : ''}{data.cost ? ` · ${data.cost}` : ''}{data.model ? ` · ${data.model}` : ''}
  </p>
</div>
<Handle type="source" position={Position.Bottom} />
<Handle type="source" id="right" position={Position.Right} />
<Handle type="source" id="under" position={Position.Bottom} style="left: 12px" />

<style>
  .agent {
    display: flex; flex-direction: column; justify-content: center; gap: 3px; padding: 8px 12px; overflow: hidden;
    border-radius: var(--radius); border: 1px solid var(--line-2); background: var(--surface); box-shadow: var(--shadow);
  }
  .agent.now { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  .top { display: flex; align-items: center; justify-content: space-between; gap: 6px; font-size: 13px; }
  .facts { font-size: 11px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
</style>
