<script lang="ts">
  import { live, route, go } from '../lib/live.svelte';
  import { pct, span } from '../lib/format';
  import type { Project } from '../lib/types';

  const projects = $derived(live.picture?.projects || []);
  const waiting = $derived((live.picture?.inbox || []).filter((item) => !live.sent[item.key]).length);
  const agents = $derived((live.picture?.runs || []).length);
  const accounts = $derived(live.picture?.accounts || []);
  const silent = $derived(live.heard ? live.now - live.skew - live.heard : 0);
  const stale = $derived(live.heard > 0 && silent > 25);

  function subline(project: Project): string {
    const p = project.pulse;
    if (!p) return '';
    const parts: string[] = [];
    if (p.goals) parts.push(`${p.goals} goal${p.goals === 1 ? '' : 's'}`);
    if (p.pipelines) parts.push(`${p.pipelines} running`);
    if (p.parked) parts.push(`${p.parked} parked`);
    if (!parts.length) return p.state === 'idle' ? 'Idle' : '';
    return parts.join(' · ');
  }

  const label: Record<string, string> = {
    waiting: 'Waiting on you',
    parked: 'Something parked',
    working: 'Working',
    open: 'Open, nothing running',
    idle: 'Idle',
  };

  const level = (used: number) => (used >= 0.9 ? 'crit' : used >= 0.7 ? 'warn' : '');
</script>

<nav class="side" aria-label="Places">
  <div class="brand">
    <span class="logo" aria-hidden="true">◧</span>
    <div>
      <h1>Factory</h1>
      <p class="pulse" class:stale role="status">
        {#if !live.heard}Connecting…
        {:else if stale}Not updated for {span(silent)}
        {:else}Live{live.picture?.release ? ` · ${live.picture.release}` : ''}{/if}
      </p>
    </div>
  </div>

  <ul class="places">
    <li>
      <button class="place" class:here={route.at.view === 'inbox'} onclick={() => go({ view: 'inbox' })}>
        <span class="glyph" aria-hidden="true">⌂</span>
        <span class="name">Inbox</span>
        {#if waiting}<span class="badge wait" aria-label="{waiting} waiting on you">{waiting}</span>{/if}
      </button>
    </li>
    <li>
      <button class="place" class:here={route.at.view === 'floor'} onclick={() => go({ view: 'floor' })}>
        <span class="glyph" aria-hidden="true">≋</span>
        <span class="name">Floor</span>
        {#if agents}<span class="badge run" aria-label="{agents} agents running">{agents}</span>{/if}
      </button>
    </li>
  </ul>

  <h2 class="heading">Projects</h2>
  <ul class="places projects">
    {#each projects as project, index (project.id)}
      {@const state = project.pulse?.state || 'idle'}
      <li>
        <button
          class="place project"
          class:here={route.at.view === 'project' && route.at.project === project.id}
          onclick={() => go({ view: 'project', project: project.id })}
          title={label[state]}
        >
          <span class="dot {state}" aria-label={label[state]}></span>
          <span class="text">
            <span class="name">{project.id}</span>
            <span class="sub">{subline(project)}</span>
          </span>
          {#if project.pulse?.waiting}
            <span class="badge wait" aria-label="{project.pulse.waiting} waiting on you">{project.pulse.waiting}</span>
          {:else if project.pulse?.agents}
            <span class="badge run" aria-label="{project.pulse.agents} agents running">{project.pulse.agents}</span>
          {/if}
          {#if index < 9}<kbd class="key" aria-hidden="true">{index + 1}</kbd>{/if}
        </button>
      </li>
    {:else}
      <li class="quiet none">No project has records yet.</li>
    {/each}
  </ul>

  <div class="accounts">
    <h2 class="heading">Accounts</h2>
    {#each accounts as account (account.name)}
      {@const limited = [account.five_hour, account.seven_day].some((w) => w.status && !w.status.startsWith('allowed'))}
      {@const blocked = Object.keys(account.blocked || {})}
      <button class="account" onclick={() => go({ view: 'floor' })} title="Open the floor for this account's windows">
        <span class="acct-name">{account.name}{#if limited}<span class="limit">{' · at its limit'}</span>{:else if blocked.length}<span class="warnish">{` · ${blocked.includes('*') ? 'blocked' : `no ${blocked.join(', ')}`}`}</span>{:else if account.runs?.length}<span class="quiet">{` · ${account.runs.length} on it`}</span>{/if}</span>
        <span class="bars">
          <span class="bar" title="5-hour window: {pct(account.five_hour.utilization)}">
            <span class="fill {level(account.five_hour.utilization)}" style:width="{Math.min(100, account.five_hour.utilization * 100)}%"></span>
          </span>
          <span class="bar" title="7-day window: {pct(account.seven_day.utilization)}">
            <span class="fill {level(account.seven_day.utilization)}" style:width="{Math.min(100, account.seven_day.utilization * 100)}%"></span>
          </span>
        </span>
      </button>
    {/each}
  </div>
</nav>

<style>
  .side {
    display: flex; flex-direction: column; gap: 4px; height: 100%;
    background: var(--side); border-right: 1px solid var(--line);
    padding: 14px 10px; overflow-y: auto;
  }
  .brand { display: flex; gap: 10px; align-items: center; padding: 2px 8px 14px; }
  .logo { font-size: 22px; color: var(--accent); }
  h1 { font-size: 15px; letter-spacing: -0.01em; }
  .pulse { font-size: 11px; color: var(--ink-3); }
  .pulse::before { content: "● "; color: var(--good); }
  .pulse.stale::before { color: var(--critical); }
  .heading {
    font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.06em;
    color: var(--ink-3); padding: 14px 10px 4px;
  }
  .places { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 1px; }
  .place {
    width: 100%; display: flex; align-items: center; gap: 10px; text-align: left;
    padding: 6px 10px; border: 0; border-radius: 6px; background: transparent; cursor: pointer;
  }
  .place:hover { background: var(--ring); }
  .place.here { background: var(--surface); box-shadow: var(--shadow); }
  .glyph { width: 16px; text-align: center; color: var(--ink-3); }
  .name { font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .place > .name { flex: 1; }
  .text { flex: 1; display: flex; flex-direction: column; min-width: 0; }
  .sub { font-size: 11px; color: var(--ink-3); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .badge {
    min-width: 20px; padding: 0 6px; border-radius: 999px; font-size: 11px; font-weight: 700;
    line-height: 18px; text-align: center; font-variant-numeric: tabular-nums;
  }
  .badge.wait { background: var(--warning-solid); color: #241a00; }
  .badge.run { background: var(--accent-soft); color: var(--accent); }
  .key { display: none; }
  .place:hover .key { display: inline; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex: none; background: var(--line-2); }
  .dot.working { background: var(--accent); box-shadow: 0 0 0 0 var(--accent); animation: ping 2s infinite; }
  .dot.waiting { background: var(--warning-solid); }
  .dot.parked { background: var(--serious); }
  .dot.open { background: var(--good); }
  @keyframes ping {
    0% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--accent) 60%, transparent); }
    70% { box-shadow: 0 0 0 6px transparent; }
    100% { box-shadow: 0 0 0 0 transparent; }
  }
  .none { padding: 6px 10px; font-size: 12px; }
  .accounts { margin-top: auto; padding-top: 10px; }
  .account {
    width: 100%; display: flex; flex-direction: column; gap: 4px; padding: 6px 10px;
    border: 0; background: transparent; text-align: left; cursor: pointer; border-radius: 6px;
  }
  .account:hover { background: var(--ring); }
  .acct-name { font-size: 12px; font-weight: 500; }
  .limit { color: var(--critical); font-weight: 400; }
  .warnish { color: var(--warning); font-weight: 400; }
  .bars { display: flex; flex-direction: column; gap: 3px; }
  .bar { height: 4px; border-radius: 2px; background: var(--line); overflow: hidden; }
  .fill { display: block; height: 100%; background: var(--accent); border-radius: 2px; }
  .fill.warn { background: var(--warning-solid); }
  .fill.crit { background: var(--critical); }
</style>
