<script lang="ts">
  import { render } from '../lib/mrkdwn';

  let { text, clamp = 0 }: { text: string | null | undefined; clamp?: number } = $props();
  let open = $state(false);
  let body: HTMLDivElement | undefined = $state();
  let tall = $state(false);
  const html = $derived(render(text));

  $effect(() => {
    void html;
    if (body && clamp) tall = body.scrollHeight > clamp + 40;
  });
</script>

<div class="md">
  <!-- The text is escaped by render(); only its own marks are markup. -->
  <div
    class="prose"
    class:clamped={clamp && !open && tall}
    bind:this={body}
    style:max-height={clamp && !open && tall ? `${clamp}px` : null}
  >{@html html}</div>
  {#if clamp && tall}
    <button class="more" onclick={() => (open = !open)}>{open ? 'Show less' : 'Show all'}</button>
  {/if}
</div>

<style>
  .clamped { overflow: hidden; mask-image: linear-gradient(to bottom, #000 75%, transparent); }
  .more {
    margin-top: 4px; border: 0; background: none; color: var(--accent); cursor: pointer;
    padding: 0; font-size: 12px;
  }
</style>
