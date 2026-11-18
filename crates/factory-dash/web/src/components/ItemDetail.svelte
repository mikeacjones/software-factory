<script lang="ts">
  // One thing waiting on the owner: what is asked, the acts that answer it,
  // and the conversation it belongs to.
  import { live, press, go } from '../lib/live.svelte';
  import { ago, clock, kindWords, short, slackLink } from '../lib/format';
  import type { Action, Item } from '../lib/types';
  import Md from './Md.svelte';
  import Thread from './Thread.svelte';
  import Composer from './Composer.svelte';

  let { item, textarea = $bindable() }: { item: Item; textarea?: HTMLTextAreaElement } = $props();

  let busy = $state<string | null>(null);
  let arming = $state<string | null>(null);
  let said = $state<{ ok: boolean; text: string } | null>(null);

  $effect(() => {
    void item.key;
    busy = null;
    arming = null;
    said = null;
  });

  const sent = $derived(live.sent[item.key]);
  const slack = $derived(slackLink(live.picture?.links?.slack, item.slack));

  export async function act(action: Action) {
    if (!live.canAct || busy || sent) return;
    if (action.confirm && arming !== action.action_id) {
      arming = action.action_id;
      return;
    }
    arming = null;
    busy = action.action_id;
    said = null;
    const done = await press(item.project, action.action_id, action.value, item.key, action.label);
    busy = null;
    said = done.ok
      ? { ok: true, text: `${action.label}: sent. It leaves the inbox once the factory has taken it in.` }
      : { ok: false, text: done.error };
  }

  const priority = (p?: number) => (p == null || p < 0 ? 'unchanged' : `P${p}`);
</script>

<article class="detail">
  <header>
    <p class="crumbs">
      <span class="kind {item.kind}">{kindWords[item.kind] || item.kind}</span>
      <button class="link" onclick={() => go({ view: 'project', project: item.project, node: item.goal ? `goal:${item.goal}` : undefined })}>
        {item.project}{#if item.goal} › <span class="mono">{short(item.goal, item.project)}</span>{/if}
      </button>
      {#if item.for_project && item.for_project !== item.project}<span class="quiet">for {item.for_project}</span>{/if}
      {#if item.at}<time class="quiet" title={clock(item.at, live.now)}>{ago(item.at, live.now)}</time>{/if}
      {#if slack}<a class="quiet" href={slack} target="_blank" rel="noopener noreferrer">Slack ↗</a>{/if}
    </p>
    <h2>{item.title}</h2>
    <p class="summary">{item.summary}</p>

    {#if item.actions.length}
      <div class="actions" role="group" aria-label="Answer">
        {#each item.actions as action, index (action.action_id)}
          <button
            class="btn {action.tone}"
            class:armed={arming === action.action_id}
            disabled={!live.canAct || !!busy || !!sent}
            onclick={() => act(action)}
            title={action.confirm || ''}
          >
            {busy === action.action_id ? 'Sending…' : arming === action.action_id ? `Really ${action.label.toLowerCase()}?` : action.label}
            <kbd>{index + 1}</kbd>
          </button>
        {/each}
      </div>
    {/if}
    {#if sent}
      <p class="note good">{sent.label} · sent {ago(sent.at, live.now)}. Waiting for the factory to take it in…</p>
    {:else if said}
      <p class="note" class:good={said.ok} class:bad={!said.ok}>{said.text}</p>
    {:else if arming}
      <p class="note">{item.actions.find((a) => a.action_id === arming)?.confirm} Press again to confirm.</p>
    {:else if !live.canAct && item.actions.length}
      <p class="note">{live.whyNot}</p>
    {/if}
  </header>

  {#each (item.sections || []).filter((s) => s.text) as section (section.label)}
    <section>
      <h3>{section.label}</h3>
      <Md text={section.text} clamp={section.label === 'Plan' || section.label === 'Evidence' ? 260 : 420} />
    </section>
  {/each}

  {#if item.edits?.length}
    <section>
      <h3>Changes to existing beads</h3>
      <table class="edits">
        <thead><tr><th>Bead</th><th>Priority</th><th>Deferral</th><th>Why</th></tr></thead>
        <tbody>
          {#each item.edits as edit (edit.bead)}
            <tr>
              <td class="mono">{short(edit.bead, item.project)}</td>
              <td>{priority(edit.priority)}</td>
              <td>{edit.defer || 'unchanged'}</td>
              <td>{edit.reason || ''}{#if edit.acceptance_criteria}<details><summary>New acceptance criteria</summary><Md text={edit.acceptance_criteria} /></details>{/if}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </section>
  {/if}

  {#if item.reply}
    <section>
      <h3>Reply</h3>
      <Composer
        project={item.project}
        channel={item.reply.channel}
        thread={item.reply.ts}
        sentKey={item.kind === 'proposal' ? undefined : item.key}
        hint={item.reply.hint || 'Goes to the goal’s thread, as a reply in Slack would.'}
        placeholder={item.kind === 'completion' ? 'Not done yet? Say what is missing…' : item.kind === 'charter' ? 'Ask for a change to the charter…' : 'Write your answer…'}
        bind:textarea
      />
    </section>
  {:else if item.kind === 'step_question'}
    <p class="note">This pipeline's driver reads its answer from Slack. {#if slack}<a href={slack} target="_blank" rel="noopener noreferrer">Answer in its thread ↗</a>{/if}</p>
  {/if}

  {#if item.goal}
    <section class="talk">
      <h3>Conversation</h3>
      <Thread where="goal" id={item.goal} />
    </section>
  {/if}
</article>

<style>
  .detail { display: flex; flex-direction: column; gap: 20px; padding: 20px 28px 40px; max-width: 860px; }
  header { display: flex; flex-direction: column; gap: 8px; }
  .crumbs { display: flex; flex-wrap: wrap; align-items: center; gap: 10px; font-size: 12px; }
  .link { border: 0; background: none; padding: 0; cursor: pointer; color: var(--ink-2); }
  .link:hover { color: var(--accent); }
  .kind {
    font-size: 11px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.05em;
    padding: 2px 8px; border-radius: 4px; background: var(--warning-soft); color: var(--warning);
  }
  .kind.proposal { background: var(--accent-soft); color: var(--accent); }
  .kind.parked, .kind.step_question { background: var(--sunk); color: var(--ink-2); }
  h2 { font-size: 20px; line-height: 1.3; letter-spacing: -0.01em; }
  .summary { color: var(--ink-2); }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 6px; }
  .actions .btn { padding: 6px 14px; font-size: 14px; }
  .btn.armed { background: var(--critical); color: #fff; border-color: var(--critical); }
  .note { font-size: 12px; color: var(--ink-2); }
  .note.good { color: var(--good); }
  .note.bad { color: var(--critical); }
  section { display: flex; flex-direction: column; gap: 6px; }
  h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-3); }
  .edits { border-collapse: collapse; font-size: 13px; width: 100%; }
  .edits th { text-align: left; font-weight: 600; color: var(--ink-3); font-size: 12px; }
  .edits th, .edits td { padding: 6px 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
  .talk :global(.thread) { max-height: 520px; padding-right: 4px; }
</style>
