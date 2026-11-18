<script lang="ts">
  // One agent's run, read as it is written: what it said, the tools it
  // called and what came back. Older runs of the same place can be chosen.
  import { drawer, closeRun } from '../lib/drawer.svelte';
  import { money } from '../lib/format';

  type Event = { k: string; text?: string; name?: string; err?: boolean; outcome?: string; cost?: number; turns?: number };

  let file = $state('');
  let events: Event[] = $state([]);
  let skipped = $state(0);
  let status = $state('');
  let feed: HTMLOListElement | undefined = $state();
  let closer: HTMLButtonElement | undefined = $state();
  let opened = $state(new Set<number>());

  $effect(() => {
    if (drawer.run) {
      file = drawer.run.file;
      queueMicrotask(() => closer?.focus());
    }
  });

  $effect(() => {
    const run = drawer.run;
    if (!run || !file) return;
    let gone = false;
    let timer = 0;
    let next: number | null = null;
    let ended = false;
    events = [];
    skipped = 0;
    opened = new Set();
    status = 'Reading…';
    const pump = async () => {
      let more = false;
      try {
        const query = new URLSearchParams({ where: run.where, id: run.id, file });
        if (next != null) query.set('from', String(next));
        const answer = await fetch(`/api/run?${query}`, { cache: 'no-store' });
        if (gone) return;
        if (!answer.ok) {
          status = 'This run\'s log cannot be read.';
        } else {
          const part = await answer.json();
          if (gone) return;
          const follow = feed ? feed.scrollTop + feed.clientHeight >= feed.scrollHeight - 60 : true;
          if (next == null && part.from > 0) skipped = Math.round(part.from / 1024);
          const fresh: Event[] = part.events.filter((event: Event) => ['say', 'tool', 'out', 'start', 'end'].includes(event.k) && (event.k !== 'out' || event.text));
          if (fresh.some((event) => event.k === 'end')) ended = true;
          events = [...events, ...fresh].slice(-2000);
          if (follow) queueMicrotask(() => feed && (feed.scrollTop = feed.scrollHeight));
          next = part.next;
          more = part.next < part.size && part.events.length > 0;
          status = ended ? 'This run has ended.' : 'Following: new lines appear as the agent writes them.';
        }
      } catch {
        status = 'The dashboard\'s server is not answering.';
      }
      if (!gone && !ended) timer = window.setTimeout(pump, more ? 0 : document.hidden ? 6000 : 1500);
    };
    pump();
    return () => {
      gone = true;
      clearTimeout(timer);
    };
  });

  function toggle(index: number) {
    const next = new Set(opened);
    if (next.has(index)) next.delete(index);
    else next.add(index);
    opened = next;
  }
</script>

<svelte:window onkeydown={(event) => { if (event.key === 'Escape' && drawer.run) closeRun(); }} />

{#if drawer.run}
  <aside class="drawer" aria-labelledby="drawer-title">
    <header>
      <div>
        <h2 id="drawer-title">{drawer.run.title}</h2>
        <p class="quiet">{drawer.run.sub}</p>
      </div>
      <button class="btn" bind:this={closer} onclick={closeRun}>Close <kbd>esc</kbd></button>
    </header>
    <div class="bar">
      <label for="drawer-run">Run</label>
      <select id="drawer-run" bind:value={file}>
        {#each [...drawer.run.files].reverse() as name (name)}<option value={name}>{name.replace(/\.jsonl$/, '')}</option>{/each}
      </select>
      <span class="quiet" role="status">{status}</span>
    </div>
    <ol class="feed" bind:this={feed}>
      {#if skipped}<li class="mark">The first {skipped} KB of this run are not shown.</li>{/if}
      {#each events as event, index (index)}
        {#if event.k === 'say'}
          <li class="say">{event.text}</li>
        {:else if event.k === 'tool'}
          <li class="tool"><b>{event.name}</b><code>{event.text}</code></li>
        {:else if event.k === 'out'}
          <li class="out" class:err={event.err} class:open={opened.has(index)}>
            <button onclick={() => toggle(index)} title="Show all of it"><pre>{event.text}</pre></button>
          </li>
        {:else if event.k === 'start'}
          <li class="mark">Began: {event.text}</li>
        {:else if event.k === 'end'}
          <li class="mark"><b>Ended ({event.outcome || 'no outcome said'}){event.turns != null ? ` · ${event.turns} turns` : ''}{event.cost != null ? ` · ${money(event.cost)}` : ''}</b>{event.text ? `\n${event.text}` : ''}</li>
        {/if}
      {/each}
    </ol>
  </aside>
{/if}

<style>
  .drawer {
    position: fixed; top: 0; right: 0; bottom: 0; width: min(760px, 100vw); z-index: 50;
    background: var(--surface); border-left: 1px solid var(--line); box-shadow: -10px 0 40px rgba(0, 0, 0, 0.18);
    display: flex; flex-direction: column;
  }
  header { display: flex; justify-content: space-between; gap: 12px; padding: 14px 18px; border-bottom: 1px solid var(--line); }
  h2 { font-size: 16px; }
  .bar { display: flex; gap: 10px; align-items: center; padding: 8px 18px; border-bottom: 1px solid var(--line); font-size: 12px; }
  select { font: inherit; background: var(--surface); color: var(--ink); border: 1px solid var(--line-2); border-radius: 6px; padding: 2px 6px; }
  .feed { list-style: none; margin: 0; padding: 12px 18px 40px; overflow: auto; flex: 1; display: flex; flex-direction: column; gap: 8px; font-size: 13px; }
  .say { white-space: pre-wrap; }
  .tool { display: flex; gap: 8px; align-items: baseline; color: var(--ink-2); }
  .tool b { font-size: 12px; color: var(--accent); }
  .tool code { white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; }
  .out button { all: unset; display: block; cursor: pointer; width: 100%; }
  .out pre {
    margin: 0; padding: 6px 10px; background: var(--sunk); border-radius: 6px; font: 12px/1.4 var(--mono);
    white-space: pre-wrap; overflow-wrap: anywhere; max-height: 7.5em; overflow: hidden;
  }
  .out.open pre { max-height: none; }
  .out.err pre { box-shadow: inset 3px 0 0 var(--critical); }
  .mark { color: var(--ink-3); font-size: 12px; white-space: pre-wrap; text-align: center; }
</style>
