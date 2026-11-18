<script lang="ts">
  // The executive, a goal or a sub-goal on a project's map.
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';
  import type { Tone } from '../../lib/format';

  type Data = {
    level: 'executive' | 'goal' | 'subgoal' | 'group';
    label: string;
    title: string;
    tone: Tone;
    word: string;
    facts: string[];
    busy: boolean;
    waiting: number;
    chosen: boolean;
    progress?: [number, number];
  };
  let { data }: NodeProps & { data: Data } = $props();
</script>

<Handle type="target" position={Position.Left} />
<div class="tier {data.level}" class:busy={data.busy} class:waiting={data.waiting > 0} class:chosen={data.chosen}>
  <div class="top">
    <span class="label">{data.label}</span>
    {#if data.waiting}<span class="ask" title="Waiting on you">{data.waiting} for you</span>{/if}
    <span class="pill {data.tone}">{data.word}</span>
  </div>
  <p class="title" title={data.title}>{data.title}</p>
  {#if data.progress && data.progress[1]}
    <div class="progress" title="{data.progress[0]} of {data.progress[1]} beads absorbed">
      <span style:width="{(data.progress[0] / data.progress[1]) * 100}%"></span>
    </div>
  {/if}
  {#if data.facts.length}<p class="facts">{data.facts.join(' · ')}</p>{/if}
</div>
<Handle type="source" position={Position.Right} />

<style>
  .tier {
    width: 270px; padding: 10px 12px; border-radius: var(--radius); background: var(--surface);
    border: 1px solid var(--line-2); box-shadow: var(--shadow); display: flex; flex-direction: column; gap: 5px;
    cursor: pointer; transition: border-color 0.15s, box-shadow 0.15s;
  }
  .tier:hover { border-color: var(--ink-3); }
  .tier.executive { border-top: 3px solid var(--ink-2); }
  .tier.goal { border-top: 3px solid var(--accent); }
  .tier.group { border-style: dashed; }
  .tier.busy { border-color: color-mix(in srgb, var(--accent) 55%, var(--line-2)); }
  .tier.waiting { border-color: var(--warning-solid); box-shadow: 0 0 0 3px var(--warning-soft), var(--shadow); }
  .tier.chosen { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft), var(--shadow); }
  .top { display: flex; align-items: center; gap: 6px; }
  .label { font-size: 10px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-3); flex: 1; }
  .ask { font-size: 10px; font-weight: 700; color: #241a00; background: var(--warning-solid); padding: 0 6px; border-radius: 999px; }
  .title {
    font-size: 13px; font-weight: 600; line-height: 1.3; color: var(--ink);
    display: -webkit-box; -webkit-line-clamp: 2; line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden;
  }
  .facts { font-size: 11px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .progress { height: 4px; background: var(--line); border-radius: 2px; overflow: hidden; }
  .progress span { display: block; height: 100%; background: var(--good); }
</style>
