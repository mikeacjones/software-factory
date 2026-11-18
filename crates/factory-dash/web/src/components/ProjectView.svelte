<script lang="ts">
  // One project: its numbers across the top, its map, and whatever on the
  // map is chosen, in full beside it.
  import { live, go } from '../lib/live.svelte';
  import { money, plural, toneOf, kindWords } from '../lib/format';
  import { findRef } from '../lib/graph';
  import type { Project } from '../lib/types';
  import ProjectGraph from './ProjectGraph.svelte';
  import Inspector from './Inspector.svelte';
  import NewMessage from './NewMessage.svelte';

  let { project, node }: { project: Project; node?: string } = $props();

  const inbox = $derived((live.picture?.inbox || []).filter((item) => item.project === project.id));
  const ref = $derived(findRef(project, node));
  const pulse = $derived(project.pulse);
  const cost = $derived((project.goals || []).reduce((sum, goal) => sum + (goal.cost || 0), 0));
  let showFinished = $state(false);
  let writing = $state(false);
  const waitingNew = $derived((live.started[project.id] || []).length);
</script>

<div class="project">
  <header>
    <div class="name">
      <h2>{project.id}</h2>
      {#if project.workflow}<a class="quiet" href={project.workflow} target="_blank" rel="noopener noreferrer">Workflow ↗</a>{/if}
      <span class="spacer"></span>
      <button class="btn primary" onclick={() => (writing = !writing)} title="A new message, as at the top of the project's channel">
        ✎ {project.executive ? 'Message the executive' : 'Open a goal'}{waitingNew ? ` (${waitingNew} waiting)` : ''}
      </button>
    </div>
    {#if writing}<NewMessage {project} onclose={() => (writing = false)} />{/if}
    <dl class="stats">
      <div><dt>Waiting on you</dt><dd class:warn={!!pulse?.waiting}>{pulse?.waiting ?? 0}</dd></div>
      <div><dt>Agents at work</dt><dd class:run={!!pulse?.agents}>{pulse?.agents ?? 0}</dd></div>
      <div><dt>Pipelines running</dt><dd>{pulse?.pipelines ?? 0}</dd></div>
      <div><dt>Parked</dt><dd class:bad={!!pulse?.parked}>{pulse?.parked ?? 0}</dd></div>
      <div><dt>Beads absorbed</dt><dd>{pulse?.done ?? 0}<span class="of">/{pulse?.beads ?? 0}</span></dd></div>
      <div><dt>Open goals' tiers</dt><dd>{money(cost)}</dd></div>
    </dl>
    {#if inbox.length}
      <div class="asks">
        {#each inbox as item (item.key)}
          <button class="ask" onclick={() => go({ view: 'inbox', item: item.key })}>
            <b>{kindWords[item.kind]}</b><span>{item.title}</span>
          </button>
        {/each}
      </div>
    {/if}
  </header>

  <div class="body" class:inspecting={!!ref}>
    <div class="canvas">
      {#if (project.goals || []).length || project.executive || (project.loose || []).length}
        {#key project.id}
          <ProjectGraph
            {project}
            {inbox}
            chosen={node}
            now={live.now}
            map={!ref}
            onchoose={(id) => go({ view: 'project', project: project.id, node: id === node ? undefined : id })}
          />
        {/key}
      {:else}
        <div class="empty">
          <h3>Nothing is open here.</h3>
          <p class="quiet">A goal appears on this map when it is opened: write in the project's channel, or approve a proposal.</p>
        </div>
      {/if}
      {#if (project.finished || []).length}
        <div class="finished">
          <button class="btn small" onclick={() => (showFinished = !showFinished)}>{plural(project.finished!.length, 'finished goal')}</button>
          {#if showFinished}
            <ul>
              {#each project.finished! as goal (goal.id)}
                {@const [tone, word] = toneOf(goal.phase)}
                <li><span class="pill {tone}">{word}</span><span class="obj">{goal.objective || goal.id}</span><span class="num quiet">{money(goal.cost)}</span></li>
              {/each}
            </ul>
          {/if}
        </div>
      {/if}
    </div>
    {#if ref}
      {#key node}
        <Inspector {project} {ref} {inbox} onclose={() => go({ view: 'project', project: project.id })} />
      {/key}
    {/if}
  </div>
</div>

<style>
  .project { display: flex; flex-direction: column; height: 100%; min-height: 0; }
  header { padding: 16px 20px 12px; border-bottom: 1px solid var(--line); background: var(--surface); display: flex; flex-direction: column; gap: 12px; }
  .name { display: flex; align-items: baseline; gap: 12px; }
  .spacer { flex: 1; }
  h2 { font-size: 20px; letter-spacing: -0.01em; }
  .stats { display: flex; flex-wrap: wrap; gap: 6px 28px; margin: 0; }
  .stats div { display: flex; flex-direction: column; }
  dt { font-size: 11px; color: var(--ink-3); }
  dd { margin: 0; font-size: 18px; font-weight: 600; font-variant-numeric: tabular-nums; }
  dd.warn { color: var(--warning); }
  dd.run { color: var(--accent); }
  dd.bad { color: var(--critical); }
  .of { font-size: 13px; color: var(--ink-3); font-weight: 400; }
  .asks { display: flex; flex-direction: column; gap: 4px; }
  .ask {
    display: flex; gap: 10px; align-items: center; text-align: left; cursor: pointer;
    border: 1px solid var(--warning-solid); background: var(--warning-soft); border-radius: 6px; padding: 6px 10px; font-size: 13px;
  }
  .ask b { color: var(--warning); white-space: nowrap; font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; }
  .ask span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .body { flex: 1; display: grid; grid-template-columns: 1fr; min-height: 0; }
  .body.inspecting { grid-template-columns: 1fr minmax(360px, 34%); }
  .canvas { position: relative; min-height: 0; }
  .empty { max-width: 420px; margin: 18vh auto; text-align: center; display: flex; flex-direction: column; gap: 8px; }
  .finished { position: absolute; left: 12px; top: 12px; z-index: 5; max-width: 520px; }
  .finished ul { list-style: none; padding: 8px; margin: 6px 0 0; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); box-shadow: var(--shadow); display: flex; flex-direction: column; gap: 6px; max-height: 50vh; overflow: auto; }
  .finished li { display: flex; gap: 8px; align-items: center; font-size: 12px; }
  .obj { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  @media (max-width: 900px) {
    .body.inspecting { grid-template-columns: 1fr; grid-template-rows: 45% 55%; }
  }
</style>
