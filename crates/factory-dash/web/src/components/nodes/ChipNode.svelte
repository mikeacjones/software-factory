<script lang="ts">
  // Something that woke a tier, or something it did, with how often.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';

  type Data = { label: string; count: number; kind: 'cause' | 'outcome'; tone?: string; size: { width: number; height: number } };
  let { data }: NodeProps & { data: Data } = $props();
</script>

<Handle type="target" position={Position.Top} />
<Handle type="target" id="left-in" position={Position.Left} />
<div class="chip {data.kind} {data.tone ?? ''}" style:width="{data.size.width}px" style:height="{data.size.height}px" title="{data.label}: {data.count}">
  <span class="label">{data.label}</span><b>×{data.count}</b>
</div>
<Handle type="source" position={Position.Bottom} />

<style>
  .chip {
    display: flex; align-items: center; gap: 6px; padding: 0 10px; border-radius: 999px; overflow: hidden;
    border: 1px solid var(--line-2); background: var(--surface); font-size: 11px;
  }
  .chip.cause { background: var(--sunk); border-style: dashed; }
  .chip.good { border-color: color-mix(in srgb, var(--good) 50%, var(--line-2)); }
  .chip.warn { border-color: var(--warning-solid); }
  .chip.bad { border-color: var(--critical); }
  .label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--ink-2); }
  b { font-variant-numeric: tabular-nums; }
</style>
