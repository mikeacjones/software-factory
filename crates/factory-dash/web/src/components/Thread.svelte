<script lang="ts">
  // A conversation between the owner and the factory, from the records,
  // read again every few seconds while it is shown.
  import { conversation, live } from '../lib/live.svelte';
  import { ago, clock } from '../lib/format';
  import type { Line } from '../lib/types';
  import Md from './Md.svelte';

  let { where, id, thread, events = true }: { where: 'goal' | 'project'; id: string; thread?: string; events?: boolean } = $props();

  let lines: Line[] = $state([]);
  let earlier = $state(false);
  let loaded = $state(false);
  let box: HTMLOListElement | undefined = $state();

  $effect(() => {
    const key = `${where}/${id}/${thread ?? ''}`;
    let gone = false;
    let timer = 0;
    loaded = false;
    lines = [];
    const look = async () => {
      const got = await conversation(where, id, thread);
      if (gone) return;
      if (got) {
        const grew = got.lines.length !== lines.length;
        const follow = box ? box.scrollTop + box.clientHeight >= box.scrollHeight - 80 : true;
        lines = got.lines;
        earlier = got.earlier;
        if (grew && (follow || !loaded)) queueMicrotask(() => box && (box.scrollTop = box.scrollHeight));
      }
      loaded = true;
      timer = window.setTimeout(look, document.hidden ? 20000 : 5000);
    };
    void key;
    look();
    return () => {
      gone = true;
      clearTimeout(timer);
    };
  });

  const shown = $derived(events ? lines : lines.filter((line) => line.who !== 'event'));
</script>

<ol class="thread" bind:this={box} aria-label="Conversation" aria-live="polite">
  {#if !loaded}
    <li class="mark">Reading the conversation…</li>
  {:else if !shown.length}
    <li class="mark">Nothing has been said here yet.</li>
  {/if}
  {#if earlier}<li class="mark">Earlier lines are not shown.</li>{/if}
  {#each shown as line, index (index)}
    {#if line.who === 'event'}
      <li class="mark">{line.text}{#if line.at} · <time title={clock(line.at, live.now)}>{ago(line.at, live.now)}</time>{/if}</li>
    {:else}
      <li class="line {line.who}" class:need={line.kind === 'need' || line.kind === 'prompt'} class:action={line.kind === 'action'}>
        <div class="who">
          {line.who === 'owner' ? 'You' : 'Factory'}
          {#if line.kind === 'need'}<span class="tag">needs you</span>{/if}
          {#if line.kind === 'prompt'}<span class="tag">asks you to decide</span>{/if}
          {#if line.at}<time title={clock(line.at, live.now)}>{ago(line.at, live.now)}</time>{/if}
        </div>
        {#if line.kind === 'action'}
          <p class="pressed">▸ {line.text}</p>
        {:else}
          <div class="bubble"><Md text={line.text} clamp={320} /></div>
        {/if}
      </li>
    {/if}
  {/each}
</ol>

<style>
  .thread {
    list-style: none; padding: 4px 0; margin: 0; display: flex; flex-direction: column; gap: 12px;
    overflow-y: auto; max-height: 100%;
  }
  .mark { align-self: center; font-size: 11px; color: var(--ink-3); text-align: center; }
  .line { display: flex; flex-direction: column; gap: 3px; max-width: 92%; }
  .line.owner { align-self: flex-end; align-items: flex-end; }
  .who { font-size: 11px; color: var(--ink-3); display: flex; gap: 8px; align-items: center; }
  .tag { color: var(--warning); font-weight: 600; }
  .bubble {
    padding: 8px 12px; border-radius: 10px; background: var(--surface); border: 1px solid var(--line);
    font-size: 13px;
  }
  .owner .bubble { background: var(--accent-soft); border-color: transparent; }
  .need .bubble { border-color: var(--warning-solid); box-shadow: inset 3px 0 0 var(--warning-solid); }
  .pressed { font-size: 12px; font-weight: 600; color: var(--accent); }
</style>
