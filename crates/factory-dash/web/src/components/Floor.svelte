<script lang="ts">
  // The whole factory at once: every agent at work, every account it runs
  // on, and each project in a line.
  import { live, go } from '../lib/live.svelte';
  import { model, money, short, span } from '../lib/format';
  import { openRun } from '../lib/drawer.svelte';
  import type { Picture, Run } from '../lib/types';
  import WindowMeter from './WindowMeter.svelte';

  const picture = $derived(live.picture as Picture | null);
  const runs = $derived(picture?.runs || []);
  const projects = $derived(picture?.projects || []);
  const accounts = $derived(picture?.accounts || []);

  function filesOf(run: Run): string[] | undefined {
    for (const project of projects) {
      if (run.where === 'project' && project.id === run.id) return project.executive?.runs;
      for (const goal of project.goals || []) {
        if (run.where === 'goal' && goal.id === run.id) return goal.runs;
        for (const sub of goal.subgoals || []) {
          for (const bead of [...(sub.beads || []), ...(sub.ship ? [sub.ship] : [])]) {
            if (run.where === 'unit' && bead.unit === run.id) return bead.runs;
          }
        }
      }
      for (const bead of project.loose || []) if (run.where === 'unit' && bead.unit === run.id) return bead.runs;
    }
    return undefined;
  }

  const what = (run: Run) => run.where === 'unit' ? (run.title || short(run.bead, run.project))
    : run.where === 'goal' ? `Goal ${short(run.goal, run.project)}` : `Executive of ${run.project}`;

  function quiet(run: Run): string {
    if (!run.active_at) return '';
    const secs = live.now - run.active_at;
    return secs < 30 ? 'writing now' : `quiet for ${span(secs)}`;
  }
</script>

<div class="floor scroll">
  <section>
    <h2>At work now <span class="count">{runs.length}</span></h2>
    {#if runs.length}
      <ul class="runs">
        {#each runs as run (`${run.where}/${run.id}/${run.file}`)}
          <li class="run">
            <span class="pill running"><b>{run.role}</b>{run.step ? ` · ${run.step}` : ''}</span>
            <button class="what" onclick={() => run.project && go({ view: 'project', project: run.project })}>
              <span class="title">{what(run)}</span>
              <span class="quiet small">{run.project} · {model(run.model)}{run.effort ? ` · ${run.effort}` : ''} · {run.account || 'account on record when it ends'}</span>
            </button>
            <span class="quiet small num">for {span(live.now - run.since)}<br>{quiet(run)}</span>
            <button class="btn" onclick={() => openRun(run, filesOf(run), run.step ? `${run.role} · ${run.step}` : run.role, what(run))}>Watch</button>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="quiet">No agent is running. The tiers wake when a pipeline ends, when you write, or when a time comes due.</p>
    {/if}
  </section>

  <section>
    <h2>Projects</h2>
    <div class="projects">
      {#each projects as project (project.id)}
        {@const p = project.pulse}
        <button class="card" onclick={() => go({ view: 'project', project: project.id })}>
          <span class="head"><span class="dot {p?.state}"></span><b>{project.id}</b>{#if p?.waiting}<span class="ask">{p.waiting} for you</span>{/if}</span>
          <span class="nums">
            <span><b class="num">{p?.agents ?? 0}</b> agents</span>
            <span><b class="num">{p?.pipelines ?? 0}</b> pipelines</span>
            <span><b class="num">{p?.goals ?? 0}</b> goals</span>
            <span><b class="num">{p?.done ?? 0}/{p?.beads ?? 0}</b> absorbed</span>
          </span>
        </button>
      {/each}
    </div>
  </section>

  <section>
    <h2>Accounts</h2>
    <div class="accounts">
      {#each accounts as account (account.name)}
        {@const blocked = Object.keys(account.blocked || {})}
        <article class="account">
          <header>
            <h3>{account.name}</h3>
            {#if account.runs?.length}<span class="pill running">{account.runs.length} on it</span>
            {:else if blocked.length}<span class="pill wait">{blocked.includes('*') ? 'Blocked: every model' : `Blocked: ${blocked.join(', ')}`}</span>
            {:else}<span class="pill todo">Idle</span>{/if}
          </header>
          {#if account.concern}<p class="concern">{account.concern}</p>{/if}
          <WindowMeter label="5-hour · all models" win={account.five_hour} />
          <WindowMeter label="7-day · all models" win={account.seven_day} per="day" />
          {#each Object.entries(account.models || {}) as [name, win] (name)}
            <WindowMeter label="{name.charAt(0).toUpperCase() + name.slice(1)} · 7-day" {win} per="day" asked />
          {/each}
          <p class="quiet small">Spent {money(account.spent_hour)} in the last hour, {money(account.spent_day)} in the last day, at list prices.</p>
        </article>
      {:else}
        <p class="quiet">The factory has no record of its accounts yet.</p>
      {/each}
    </div>
  </section>
</div>

<style>
  .floor { height: 100%; padding: 20px 24px 40px; display: flex; flex-direction: column; gap: 28px; }
  h2 { font-size: 16px; margin-bottom: 10px; display: flex; gap: 8px; align-items: center; }
  .count { font-size: 12px; background: var(--accent-soft); color: var(--accent); padding: 0 8px; border-radius: 999px; }
  .small { font-size: 12px; }
  .runs { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 6px; }
  .run {
    display: grid; grid-template-columns: minmax(150px, 220px) 1fr auto auto; gap: 14px; align-items: center;
    background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); padding: 10px 14px;
  }
  .run > .pill { justify-self: start; }
  .what { display: flex; flex-direction: column; text-align: left; border: 0; background: none; padding: 0; cursor: pointer; min-width: 0; }
  .what .title { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .projects { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 10px; }
  .card {
    text-align: left; cursor: pointer; background: var(--surface); border: 1px solid var(--line);
    border-radius: var(--radius); padding: 12px 14px; display: flex; flex-direction: column; gap: 8px;
  }
  .card:hover { border-color: var(--ink-3); }
  .head { display: flex; align-items: center; gap: 8px; }
  .head b { flex: 1; }
  .ask { font-size: 11px; font-weight: 700; color: #241a00; background: var(--warning-solid); padding: 0 7px; border-radius: 999px; }
  .nums { display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 12px; color: var(--ink-3); }
  .nums b { color: var(--ink); }
  .dot { width: 9px; height: 9px; border-radius: 50%; background: var(--line-2); }
  .dot.working { background: var(--accent); }
  .dot.waiting { background: var(--warning-solid); }
  .dot.parked { background: var(--serious); }
  .dot.open { background: var(--good); }
  .accounts { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 12px; }
  .account { background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); padding: 14px; display: flex; flex-direction: column; gap: 12px; }
  .account header { display: flex; justify-content: space-between; align-items: center; }
  .account h3 { font-size: 15px; }
  .concern { font-size: 12px; color: var(--warning); }
  @media (max-width: 700px) {
    .run { grid-template-columns: 1fr auto; }
  }
</style>
