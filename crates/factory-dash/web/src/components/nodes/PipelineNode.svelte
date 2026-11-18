<script lang="ts">
  // A bead drawn with its whole pipeline: a frame whose header is the
  // bead, holding its flow's steps, which Svelte Flow places inside it.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';
  import type { Tone } from '../../lib/format';

  type Data = {
    id: string;
    title: string;
    tone: Tone;
    word: string;
    where: string;
    going: boolean;
    ship: boolean;
    asks: boolean;
    chosen: boolean;
    cost: string;
    flow?: string;
    size: { width: number; height: number };
    ontoggle?: () => void;
  };
  let { data, targetPosition = Position.Top, sourcePosition = Position.Bottom }: NodeProps & { data: Data } = $props();
</script>

<Handle type="target" position={targetPosition} />
<div
  class="frame"
  class:going={data.going && data.tone === 'running'}
  class:asks={data.asks}
  class:chosen={data.chosen}
  class:ship={data.ship}
  style:width="{data.size.width}px"
  style:height="{data.size.height}px"
>
  <header>
    <span class="id mono">{data.ship ? 'ship' : data.id}</span>
    <span class="title" title={data.title}>{data.title}</span>
    <span class="pill {data.tone}">{data.word}</span>
    {#if data.ontoggle}<button class="toggle nodrag" title="Fold to a card" aria-label="Fold to a card" onclick={(event) => { event.stopPropagation(); data.ontoggle?.(); }}>⤡</button>{/if}
  </header>
  <p class="where">{data.where}{data.cost ? ` · ${data.cost}` : ''}</p>
</div>
<Handle type="source" position={sourcePosition} />

<style>
  .frame {
    border-radius: 12px; border: 1px solid var(--line-2); padding: 8px 12px; overflow: hidden;
    background: color-mix(in srgb, var(--surface) 55%, transparent); cursor: grab;
  }
  .frame.ship { border-style: dashed; }
  .frame.going { border-color: color-mix(in srgb, var(--accent) 60%, var(--line-2)); }
  .frame.asks { border-color: var(--warning-solid); box-shadow: 0 0 0 3px var(--warning-soft); }
  .frame.chosen { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  header { display: flex; align-items: center; gap: 8px; }
  .id { font-size: 11px; color: var(--ink-3); white-space: nowrap; }
  .title { flex: 1; font-weight: 600; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .where { font-size: 11px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .toggle {
    border: 1px solid var(--line-2); background: var(--surface); color: var(--ink-2); border-radius: 5px;
    width: 22px; height: 20px; padding: 0; font-size: 12px; line-height: 1; cursor: pointer; flex: none;
  }
  .toggle:hover { background: var(--accent-soft); color: var(--accent); border-color: var(--accent); }
</style>
