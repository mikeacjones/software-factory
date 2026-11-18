<script lang="ts">
  // A new message to a project, as one at the top of its channel would be:
  // to its executive, or, where it has none, the start of a new goal.
  import { live, startConversation } from '../lib/live.svelte';
  import { ago } from '../lib/format';
  import type { Project } from '../lib/types';

  let { project, onclose }: { project: Project; onclose: () => void } = $props();
  const to = $derived(project.executive ? 'the executive' : 'a new goal\'s director');

  let text = $state('');
  let sending = $state(false);
  let said = $state<{ ok: boolean; text: string } | null>(null);
  let box: HTMLTextAreaElement | undefined = $state();

  $effect(() => {
    queueMicrotask(() => box?.focus());
  });

  async function send() {
    const body = text.trim();
    if (!body || sending || !live.canAct) return;
    sending = true;
    said = null;
    const done = await startConversation(project.id, body);
    sending = false;
    if (done.ok) {
      text = '';
      said = { ok: true, text: `Sent to ${to}. Its answer appears in the conversation once it has run.` };
    } else {
      said = { ok: false, text: done.error };
    }
  }
</script>

<div class="new">
  <div class="head">
    <b>{project.executive ? 'Message the executive' : 'Open a goal'}</b>
    <span class="quiet">as a new message in #{project.id} would be</span>
    <button class="btn ghost small" onclick={onclose} aria-label="Close">✕</button>
  </div>
  <textarea
    bind:this={box}
    bind:value={text}
    rows="3"
    disabled={!live.canAct}
    placeholder={project.executive ? 'Ask what is going on, or say what you want done…' : 'Say what the goal is for…'}
    onkeydown={(event) => {
      if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); send(); }
      if (event.key === 'Escape') onclose();
    }}
  ></textarea>
  <div class="row">
    <span class="hint">
      {#if !live.canAct}{live.whyNot}{:else if said}<span class:bad={!said.ok}>{said.text}</span>{:else}It goes to {to}, and into Slack as from the dashboard.{/if}
    </span>
    <button class="btn primary" onclick={send} disabled={!text.trim() || sending || !live.canAct}>{sending ? 'Sending…' : 'Send'} <kbd>⌘↵</kbd></button>
  </div>
  {#each live.started[project.id] || [] as one (one.thread || one.at)}
    <p class="pending">⏳ “{one.text.length > 90 ? `${one.text.slice(0, 89)}…` : one.text}” · sent {ago(one.at, live.now)}, waiting for {to}</p>
  {/each}
</div>

<style>
  .new { display: flex; flex-direction: column; gap: 6px; border: 1px solid var(--accent); border-radius: var(--radius); background: var(--surface); padding: 10px 12px; box-shadow: 0 0 0 3px var(--accent-soft); }
  .head { display: flex; gap: 10px; align-items: baseline; }
  .head .quiet { flex: 1; font-size: 12px; }
  textarea { border: 1px solid var(--line-2); border-radius: 6px; padding: 8px; background: var(--plane); color: var(--ink); font: 13px/1.45 var(--sans); resize: vertical; }
  .row { display: flex; justify-content: space-between; align-items: center; gap: 10px; }
  .hint { font-size: 12px; color: var(--ink-3); }
  .bad { color: var(--critical); }
  .pending { font-size: 12px; color: var(--ink-2); }
</style>
