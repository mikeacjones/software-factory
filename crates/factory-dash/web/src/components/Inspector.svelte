<script lang="ts">
  // What a node of a project's map is, in full: a bead with its pipeline,
  // a sub-goal with its foreman, a goal with its conversation, or the
  // executive with its conversations with the owner.
  import { live, go } from '../lib/live.svelte';
  import { ago, beadState, money, plural, short, slackLink, toneOf, kindWords } from '../lib/format';
  import { beadWhere, type Ref } from '../lib/graph';
  import { openRun } from '../lib/drawer.svelte';
  import type { Item, Project } from '../lib/types';
  import Md from './Md.svelte';
  import Thread from './Thread.svelte';
  import Composer from './Composer.svelte';
  import PipelineGraph from './PipelineGraph.svelte';

  let { project, ref, inbox, onclose }: { project: Project; ref: Ref; inbox: Item[]; onclose: () => void } = $props();

  let talk = $state<string | null>(null);
  let wide = $state(false);
  const slack = $derived(live.picture?.links?.slack);
  const prLabel = (url: string) => `PR #${String(url).split('/').pop()}`;
  const asks = (goal: string) => inbox.filter((item) => item.goal === goal);
  /** The first sentence of a long text, for a heading. */
  function headline(text?: string): string {
    const first = String(text || '').split(/(?<=[.!?])\s|\n/)[0].trim();
    return first.length > 160 ? `${first.slice(0, 157)}…` : first;
  }
</script>

<svelte:window onkeydown={(event) => { if (event.key === 'Escape' && wide) { wide = false; event.stopPropagation(); } }} />

<aside class="inspector scroll" aria-label="Details">
  <button class="btn ghost close" onclick={onclose} aria-label="Close the details">✕</button>

  {#if ref.level === 'bead'}
    {@const bead = ref.bead}
    {@const [tone, word] = beadState(bead)}
    <p class="label">{ref.sub?.ship === bead ? 'Ship of a sub-goal' : 'Bead'} · <span class="mono id">{short(bead.bead, project.id)}</span></p>
    <h2>{bead.title || bead.bead}</h2>
    <p class="row"><span class="pill {tone}">{word}</span><span class="quiet">{beadWhere(bead, live.now)}</span></p>
    <div class="buttons">
      {#if bead.run}
        <button class="btn primary" onclick={() => openRun(bead.run!, bead.runs, `${bead.run!.role} · ${bead.run!.step}`, bead.title || bead.bead)}>Watch the run</button>
      {:else if bead.runs?.length && bead.unit}
        <button class="btn" onclick={() => openRun({ where: 'unit', id: bead.unit!, file: bead.runs![bead.runs!.length - 1] }, bead.runs, 'Runs of this bead', bead.title || bead.bead)}>Read its runs</button>
      {/if}
      {#if bead.pull_request}<a class="btn" href={bead.pull_request} target="_blank" rel="noopener noreferrer">{prLabel(bead.pull_request)} ↗</a>{/if}
      {#if bead.workflow}<a class="btn" href={bead.workflow} target="_blank" rel="noopener noreferrer">Workflow ↗</a>{/if}
      {#if slackLink(slack, bead.thread)}<a class="btn" href={slackLink(slack, bead.thread)} target="_blank" rel="noopener noreferrer">Thread ↗</a>{/if}
    </div>
    {#if bead.asked}
      <section class="callout">
        <h3>Step {bead.asked.step} asks you</h3>
        <Md text={bead.asked.question} />
        <p class="quiet small">Its driver reads the answer from its Slack thread.</p>
      </section>
    {/if}
    {#if bead.parked}
      <section class="callout"><h3>Why it parked</h3><Md text={bead.parked} clamp={200} /></section>
    {/if}
    <section>
      <h3 class="split">Pipeline{bead.flow ? ` · ${bead.flow}` : ''}{#if bead.steps?.length}<button class="link" onclick={() => (wide = true)}>Open large ↗</button>{/if}</h3>
      <PipelineGraph {bead} />
    </section>
    {#if wide}
      <div class="overlay" role="dialog" aria-label="The pipeline">
        <div class="sheet">
          <header class="sheet-top">
            <h2>{bead.title || bead.bead} <span class="quiet">· {bead.flow}</span></h2>
            <button class="btn" onclick={() => (wide = false)}>Close <kbd>esc</kbd></button>
          </header>
          <PipelineGraph {bead} direction="LR" height="100%" />
        </div>
      </div>
    {/if}
    <dl>
      {#if bead.cost}<dt>Cost so far</dt><dd>{money(bead.cost)}</dd>{/if}
      {#if bead.branch}<dt>Branch</dt><dd class="mono">{bead.branch}</dd>{/if}
      {#if bead.unit}<dt>Unit</dt><dd class="mono">{bead.unit}</dd>{/if}
      {#if bead.detail}<dt>Last word</dt><dd>{bead.detail}</dd>{/if}
    </dl>

  {:else if ref.level === 'subgoal'}
    {@const sub = ref.sub}
    {@const [tone, word] = toneOf(sub.state)}
    {@const foreman = sub.foreman || {}}
    <p class="label">Sub-goal of <button class="link" onclick={() => go({ view: 'project', project: project.id, node: `goal:${ref.goal.id}` })}>{short(ref.goal.id, project.id)}</button></p>
    <h2>{sub.title || sub.id}</h2>
    <p class="row"><span class="pill {tone}">{word}</span>{#if sub.branch}<span class="mono quiet">{sub.branch}</span>{/if}</p>
    <div class="buttons">
      {#if sub.pull_request}<a class="btn" href={sub.pull_request} target="_blank" rel="noopener noreferrer">{prLabel(sub.pull_request)} ↗</a>{/if}
    </div>
    <section>
      <h3>Foreman</h3>
      <p>{plural(foreman.wakes, 'wake')}{foreman.inbox ? ` · ${foreman.inbox} unread` : ''}{foreman.failures ? ` · ${plural(foreman.failures, 'failure')}` : ''}{foreman.waiting ? ` · reports ${foreman.waiting}` : ''}</p>
      {#if foreman.message}<Md text={foreman.message} clamp={180} />{/if}
    </section>
    {#if sub.objective}<section><h3>What it is for</h3><Md text={sub.objective} clamp={220} /></section>{/if}
    <section>
      <h3>Beads</h3>
      <ul class="list">
        {#each sub.beads || [] as bead (bead.unit || bead.bead)}
          {@const [btone, bword] = beadState(bead)}
          <li>
            <button class="link line" onclick={() => go({ view: 'project', project: project.id, node: `bead:${ref.goal.id}/${sub.id}/${bead.unit || bead.bead}` })}>
              <span class="pill {btone}">{bword}</span><span class="name">{bead.title || short(bead.bead, project.id)}</span>
            </button>
          </li>
        {/each}
      </ul>
    </section>

  {:else if ref.level === 'goal'}
    {@const goal = ref.goal}
    {@const [tone, word] = toneOf(goal.phase)}
    <p class="label">Goal · <span class="mono id">{short(goal.id, project.id)}</span></p>
    <h2>{headline(goal.objective) || 'A goal being shaped'}</h2>
    <p class="row"><span class="pill {tone}">{word}</span><span class="quiet">started {ago(goal.started_at, live.now)} · {money(goal.cost)} on its tiers · {plural(goal.wakes, 'wake')}</span></p>
    <div class="buttons">
      {#if goal.run}
        <button class="btn primary" onclick={() => openRun(goal.run!, goal.runs, `${goal.run!.role} at work`, `Goal ${goal.id}`)}>Watch the {goal.run.role}</button>
      {:else if goal.runs?.length}
        <button class="btn" onclick={() => openRun({ where: 'goal', id: goal.id, file: goal.runs![goal.runs!.length - 1] }, goal.runs, "Runs of the goal's tiers", `Goal ${goal.id}`)}>Past runs</button>
      {/if}
      {#if slackLink(slack, goal.thread)}<a class="btn" href={slackLink(slack, goal.thread)} target="_blank" rel="noopener noreferrer">Slack ↗</a>{/if}
    </div>
    {#each asks(goal.id) as item (item.key)}
      <button class="callout ask" onclick={() => go({ view: 'inbox', item: item.key })}>
        <b>{kindWords[item.kind]}</b> {item.title} <span class="go">Answer in the inbox →</span>
      </button>
    {/each}
    {#if goal.objective && headline(goal.objective) !== goal.objective}<section><h3>Objective</h3><Md text={goal.objective} clamp={160} /></section>{/if}
    {#if goal.stop_when}<section><h3>Done when</h3><Md text={goal.stop_when} clamp={160} /></section>{/if}
    <section class="talk">
      <h3>Conversation</h3>
      <Thread where="goal" id={goal.id} />
      {#if goal.thread}
        <Composer project={project.id} channel={goal.thread.channel} thread={goal.thread.ts} placeholder="Write to the director…" hint="Wakes the director, as a reply in the goal's thread would." />
      {/if}
    </section>

  {:else if ref.level === 'executive'}
    {@const exec = project.executive!}
    <p class="label">Executive · {project.id}</p>
    <h2>{exec.run ? `${exec.run.role} at work` : 'Asleep until you write'}</h2>
    <p class="row quiet">{plural(exec.wakes, 'wake')} · {money(exec.cost)}{exec.memos ? ` · ${plural(exec.memos, 'memo')}` : ''}{exec.waiting ? ` · ${exec.waiting} waiting for an account` : ''}</p>
    <div class="buttons">
      {#if exec.run}
        <button class="btn primary" onclick={() => openRun(exec.run!, exec.runs, 'The executive at work', `Project ${project.id}`)}>Watch</button>
      {:else if exec.runs?.length}
        <button class="btn" onclick={() => openRun({ where: 'project', id: project.id, file: exec.runs![exec.runs!.length - 1] }, exec.runs, 'Runs of the executive', `Project ${project.id}`)}>Past runs</button>
      {/if}
    </div>
    {#if exec.pending?.length}
      <section>
        <h3>To close once another closes</h3>
        <ul class="list">{#each exec.pending as one (one.bead)}<li><span class="mono">{short(one.bead, project.id)}</span> as {one.as}, once <span class="mono">{short(one.once_closed, project.id)}</span> is closed</li>{/each}</ul>
      </section>
    {/if}
    <section>
      <h3>Conversations with you</h3>
      <ul class="list">
        {#each exec.conversations || [] as one (one.ts)}
          <li>
            <button class="link line" class:on={talk === one.ts} onclick={() => (talk = talk === one.ts ? null : one.ts)}>
              <span class="when">{ago(one.at, live.now)}</span>
              <span class="name">{one.first || '(no words)'}</span>
              {#if one.goal}<span class="pill bare done">became a goal</span>{/if}
            </button>
            {#if talk === one.ts}
              <div class="talk">
                <Thread where="project" id={project.id} thread={one.ts} />
                {#if !one.goal}<Composer project={project.id} channel={one.channel || ''} thread={one.ts} placeholder="Reply to the executive…" />{/if}
              </div>
            {/if}
          </li>
        {:else}
          <li class="quiet">None yet.</li>
        {/each}
      </ul>
    </section>

  {:else}
    <p class="label">By hand</p>
    <h2>Pipelines run in no goal</h2>
    <p class="quiet">Started with <code>factory run</code>. Choose one on the map to see its pipeline.</p>
  {/if}
</aside>

<style>
  .inspector {
    position: relative; height: 100%; padding: 18px 20px 40px; background: var(--surface);
    border-left: 1px solid var(--line); display: flex; flex-direction: column; gap: 14px;
  }
  .close { position: absolute; top: 10px; right: 10px; }
  .id { text-transform: none; letter-spacing: 0; }
  .split { display: flex; justify-content: space-between; align-items: baseline; }
  .split .link { text-transform: none; letter-spacing: 0; font-size: 12px; }
  .overlay { position: fixed; inset: 0; z-index: 60; background: rgba(0, 0, 0, 0.35); display: flex; padding: 3vh 3vw; }
  .sheet { flex: 1; background: var(--surface); border-radius: 12px; box-shadow: var(--shadow); display: flex; flex-direction: column; overflow: hidden; }
  .sheet-top { display: flex; justify-content: space-between; align-items: center; padding: 12px 16px; border-bottom: 1px solid var(--line); }
  .sheet-top h2 { font-size: 15px; }
  .sheet :global(.pipeline) { flex: 1; border: 0; border-radius: 0; }
  .label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-3); padding-right: 30px; }
  h2 { font-size: 17px; line-height: 1.3; }
  h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-3); margin-bottom: 6px; }
  .row { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; font-size: 12px; }
  .buttons { display: flex; gap: 6px; flex-wrap: wrap; }
  .buttons a.btn { text-decoration: none; }
  .callout { border: 1px solid var(--warning-solid); background: var(--warning-soft); border-radius: var(--radius); padding: 10px 12px; }
  .callout.ask { text-align: left; cursor: pointer; display: flex; flex-direction: column; gap: 2px; color: var(--ink); }
  .go { font-size: 12px; color: var(--accent); }
  .small { font-size: 11px; margin-top: 4px; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 4px 12px; font-size: 12px; }
  dt { color: var(--ink-3); }
  dd { margin: 0; overflow-wrap: anywhere; }
  .list { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 2px; font-size: 13px; }
  .link { border: 0; background: none; padding: 0; color: var(--accent); cursor: pointer; font: inherit; }
  .line { display: flex; gap: 8px; align-items: center; color: var(--ink); width: 100%; text-align: left; padding: 4px 6px; border-radius: 6px; }
  .line:hover, .line.on { background: var(--sunk); }
  .line .name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .when { font-size: 11px; color: var(--ink-3); white-space: nowrap; min-width: 70px; }
  .talk { display: flex; flex-direction: column; gap: 10px; }
  .talk :global(.thread) { max-height: 440px; }
</style>
