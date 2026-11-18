<script lang="ts">
  // Everything waiting on the owner, newest first, and the one chosen.
  // j and k move, 1 to 9 press the chosen item's buttons, r replies.
  import { live, route, go } from '../lib/live.svelte';
  import { ago, kindWords, plural } from '../lib/format';
  import ItemDetail from './ItemDetail.svelte';

  let filter = $state<string>('all');
  let detail: ReturnType<typeof ItemDetail> | undefined = $state();
  let textarea: HTMLTextAreaElement | undefined = $state();

  const all = $derived(live.picture?.inbox || []);
  const projects = $derived([...new Set(all.map((item) => item.project))]);
  const items = $derived(filter === 'all' ? all : all.filter((item) => item.project === filter));
  const chosenKey = $derived(route.at.view === 'inbox' ? route.at.item : undefined);
  const chosen = $derived(items.find((item) => item.key === chosenKey) || items[0]);
  const agents = $derived((live.picture?.runs || []).length);
  const working = $derived((live.picture?.projects || []).filter((p) => p.pulse?.state === 'working').map((p) => p.id));

  function move(by: number) {
    if (!items.length) return;
    const at = Math.max(0, items.findIndex((item) => item.key === chosen?.key));
    const next = items[Math.min(items.length - 1, Math.max(0, at + by))];
    go({ view: 'inbox', item: next.key });
    queueMicrotask(() => document.getElementById(`row-${next.key}`)?.scrollIntoView({ block: 'nearest' }));
  }

  function keys(event: KeyboardEvent) {
    const typing = event.target instanceof HTMLElement && /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName);
    if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === 'j' || event.key === 'ArrowDown') { move(1); event.preventDefault(); }
    else if (event.key === 'k' || event.key === 'ArrowUp') { move(-1); event.preventDefault(); }
    else if (event.key === 'r' && textarea) { textarea.focus(); event.preventDefault(); }
    else if (/^[1-9]$/.test(event.key) && chosen) {
      const action = chosen.actions[Number(event.key) - 1];
      if (action) { detail?.act(action); event.preventDefault(); }
    }
  }
</script>

<svelte:window onkeydown={keys} />

<div class="inbox">
  <div class="list">
    <header>
      <h2>Inbox</h2>
      <p class="quiet">{all.length ? `${plural(all.length, 'thing')} waiting on you` : 'Nothing waiting on you'}</p>
      {#if projects.length > 1}
        <div class="filters" role="tablist" aria-label="Project">
          <button class="chip" class:on={filter === 'all'} onclick={() => (filter = 'all')}>All <b>{all.length}</b></button>
          {#each projects as project (project)}
            <button class="chip" class:on={filter === project} onclick={() => (filter = project)}>
              {project} <b>{all.filter((item) => item.project === project).length}</b>
            </button>
          {/each}
        </div>
      {/if}
    </header>
    <ol class="rows scroll">
      {#each items as item (item.key)}
        {@const sent = live.sent[item.key]}
        <li id="row-{item.key}">
          <button class="row" class:here={chosen?.key === item.key} class:sent onclick={() => go({ view: 'inbox', item: item.key })}>
            <span class="top">
              <span class="kind {item.kind}">{kindWords[item.kind] || item.kind}</span>
              <span class="when">{item.at ? ago(item.at, live.now) : ''}</span>
            </span>
            <span class="title">{item.title}</span>
            <span class="where">{sent ? `${sent.label} · waiting for the factory` : `${item.project}${item.goal ? ` · ${item.goal.replace(`${item.project}-`, '')}` : ''}`}</span>
          </button>
        </li>
      {/each}
    </ol>
    <footer class="keys quiet"><kbd>j</kbd><kbd>k</kbd> move · <kbd>1</kbd>–<kbd>9</kbd> answer · <kbd>r</kbd> reply</footer>
  </div>

  <div class="pane scroll">
    {#if chosen}
      {#key chosen.key}
        <ItemDetail item={chosen} bind:this={detail} bind:textarea />
      {/key}
    {:else}
      <div class="empty">
        <p class="big">✓</p>
        <h3>Nothing is waiting on you.</h3>
        <p class="quiet">
          {#if agents}{plural(agents, 'agent')} at work{working.length ? ` in ${working.join(', ')}` : ''}. {/if}
          Charters to start, completions to accept, questions and proposals appear here as the factory asks them.
        </p>
        <button class="btn" onclick={() => go({ view: 'floor' })}>See the floor</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .inbox { display: grid; grid-template-columns: minmax(280px, 360px) 1fr; height: 100%; min-height: 0; }
  .list { display: flex; flex-direction: column; border-right: 1px solid var(--line); min-height: 0; background: var(--surface); }
  header { padding: 18px 16px 10px; display: flex; flex-direction: column; gap: 4px; border-bottom: 1px solid var(--line); }
  h2 { font-size: 18px; }
  .filters { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 8px; }
  .chip { border: 1px solid var(--line-2); background: transparent; border-radius: 999px; padding: 1px 10px; font-size: 12px; cursor: pointer; }
  .chip.on { background: var(--ink); color: var(--plane); border-color: var(--ink); }
  .chip b { font-weight: 600; opacity: 0.7; margin-left: 2px; }
  .rows { list-style: none; padding: 6px; margin: 0; flex: 1; display: flex; flex-direction: column; gap: 2px; }
  .row {
    width: 100%; text-align: left; border: 0; background: transparent; border-radius: 6px; padding: 10px 12px;
    display: flex; flex-direction: column; gap: 3px; cursor: pointer; border-left: 3px solid transparent;
  }
  .row:hover { background: var(--sunk); }
  .row.here { background: var(--accent-soft); border-left-color: var(--accent); }
  .row.sent { opacity: 0.55; }
  .top { display: flex; justify-content: space-between; gap: 8px; }
  .kind { font-size: 10px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.05em; color: var(--warning); }
  .kind.proposal { color: var(--accent); }
  .kind.parked, .kind.step_question { color: var(--ink-3); }
  .when { font-size: 11px; color: var(--ink-3); white-space: nowrap; }
  .title { font-weight: 550; line-height: 1.3; display: -webkit-box; -webkit-line-clamp: 2; line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
  .where { font-size: 12px; color: var(--ink-3); }
  .keys { padding: 8px 14px; font-size: 11px; border-top: 1px solid var(--line); display: flex; gap: 4px; align-items: center; }
  .pane { min-height: 0; }
  .empty { max-width: 440px; margin: 14vh auto; text-align: center; display: flex; flex-direction: column; gap: 10px; align-items: center; }
  .big { font-size: 40px; color: var(--good); }
  @media (max-width: 760px) {
    .inbox { grid-template-columns: 1fr; grid-template-rows: auto 1fr; }
    .list { max-height: 40vh; border-right: 0; border-bottom: 1px solid var(--line); }
    .keys { display: none; }
  }
</style>
