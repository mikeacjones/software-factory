<script lang="ts">
  // A reply in a thread, sent as the owner's message there would be.
  import { live, reply } from '../lib/live.svelte';

  let {
    project,
    channel,
    thread,
    placeholder = 'Write a reply…',
    hint = '',
    sentKey,
    textarea = $bindable(),
  }: {
    project: string;
    channel: string;
    thread: string;
    placeholder?: string;
    hint?: string;
    sentKey?: string;
    textarea?: HTMLTextAreaElement;
  } = $props();

  // A draft survives the item being left and come back to.
  const drafts: Record<string, string> = (globalThis as any).__drafts ??= {};
  const key = $derived(`${project}/${thread}`);
  let text = $state('');
  let sending = $state(false);
  let said = $state<{ ok: boolean; text: string } | null>(null);

  $effect(() => {
    text = drafts[key] || '';
    said = null;
  });
  $effect(() => {
    drafts[key] = text;
  });

  async function send() {
    const body = text.trim();
    if (!body || sending || !live.canAct) return;
    sending = true;
    said = null;
    const done = await reply(project, channel, thread, body, sentKey);
    sending = false;
    if (done.ok) {
      text = '';
      said = { ok: true, text: 'Sent. The factory reads it on its next turn; its answer appears here.' };
    } else {
      said = { ok: false, text: done.error };
    }
  }

  function keys(event: KeyboardEvent) {
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      send();
    }
    if (event.key === 'Escape') textarea?.blur();
  }
</script>

<div class="composer">
  <textarea
    bind:this={textarea}
    bind:value={text}
    onkeydown={keys}
    rows="3"
    {placeholder}
    disabled={!live.canAct}
    aria-label="Reply"
  ></textarea>
  <div class="row">
    <span class="hint">
      {#if !live.canAct}{live.whyNot}{:else if said}<span class:bad={!said.ok}>{said.text}</span>{:else}{hint}{/if}
    </span>
    <button class="btn primary" onclick={send} disabled={!text.trim() || sending || !live.canAct}>
      {sending ? 'Sending…' : 'Reply'} <kbd>⌘↵</kbd>
    </button>
  </div>
</div>

<style>
  .composer {
    border: 1px solid var(--line-2); border-radius: var(--radius); background: var(--surface);
    padding: 8px; display: flex; flex-direction: column; gap: 6px;
  }
  .composer:focus-within { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  textarea {
    border: 0; outline: 0; resize: vertical; background: transparent; color: var(--ink);
    font: 13px/1.45 var(--sans); min-height: 54px;
  }
  .row { display: flex; align-items: center; justify-content: space-between; gap: 10px; }
  .hint { font-size: 12px; color: var(--ink-3); }
  .bad { color: var(--critical); }
</style>
