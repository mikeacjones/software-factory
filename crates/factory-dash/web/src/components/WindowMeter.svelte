<script lang="ts">
  // One window of an account: how much is used, how fast lately, whether
  // it runs out before it resets, and its trail so far as a small chart.
  import { live } from '../lib/live.svelte';
  import { clock, pct, span, ago } from '../lib/format';
  import type { Window } from '../lib/types';

  let { label, win, per = 'hour', asked = false }: { label: string; win: Window; per?: 'hour' | 'day'; asked?: boolean } = $props();

  const used = $derived(win.utilization || 0);
  const level = $derived(used >= 0.9 ? 'crit' : used >= 0.7 ? 'warn' : '');
  const open = $derived(win.resets_at != null);
  const start = $derived(open ? (win.resets_at as number) - win.length : 0);
  const gone = $derived(open ? Math.min(1, Math.max(0, (live.now - start) / win.length)) : 0);
  const rate = $derived(win.per_hour == null ? null : win.per_hour * (per === 'day' ? 24 : 1));
  const limited = $derived(!!win.status && !win.status.startsWith('allowed'));

  const W = 300, H = 44, TOP = 4, BASE = 40;
  const points = $derived((win.trail || []).filter(([at]) => open && at >= start && at <= (win.resets_at as number)));
  const x = (at: number) => ((at - start) / win.length) * W;
  const y = (share: number) => BASE - Math.min(1, Math.max(0, share)) * (BASE - TOP);
  const line = $derived(points.map(([at, share]) => `${x(at).toFixed(1)},${y(share).toFixed(1)}`).join(' '));
</script>

<div class="window">
  <div class="top"><span class="label">{label}</span><span class="value num">{pct(used)}</span></div>
  <div class="meter {level}" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(used * 100)} aria-label="{label} used">
    <span class="fill" style:width="{Math.min(100, used * 100)}%"></span>
    {#if open}<span class="pace" style:left="{gone * 100}%" title="How much of the window's time has gone"></span>{/if}
  </div>
  {#if points.length > 1}
    <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" class="spark" role="img" aria-label="Share used over the window so far">
      <line class="even" x1="0" y1={BASE} x2={W} y2={TOP} />
      <polygon class="area" points="{x(points[0][0]).toFixed(1)},{BASE} {line} {x(points[points.length - 1][0]).toFixed(1)},{BASE}" />
      <polyline class="path" points={line} />
    </svg>
  {/if}
  <p class="note">
    {#if limited}<span class="crit">At its limit{open ? ` until ${clock(win.resets_at as number, live.now)}` : ''}.</span>
    {:else if win.full_at}<span class="warnline">Full at {clock(win.full_at, live.now)} at this rate, {span((win.resets_at as number) - win.full_at)} before it resets.</span>
    {:else if open}Resets in {span((win.resets_at as number) - live.now)}.{#if asked && win.read_at}{` Read ${ago(win.read_at, live.now)}.`}{:else if rate != null && rate > 0}{` ${pct(rate)} a${per === 'day' ? ' day' : 'n hour'} lately.`}{/if}
    {:else}No window open.{/if}
  </p>
</div>

<style>
  .window { display: flex; flex-direction: column; gap: 4px; }
  .top { display: flex; justify-content: space-between; font-size: 12px; }
  .label { color: var(--ink-2); }
  .value { font-weight: 600; }
  .meter { position: relative; height: 8px; border-radius: 4px; background: var(--line); }
  .fill { position: absolute; inset: 0 auto 0 0; border-radius: 4px; background: var(--accent); }
  .meter.warn .fill { background: var(--warning-solid); }
  .meter.crit .fill { background: var(--critical); }
  .pace { position: absolute; top: -3px; bottom: -3px; width: 2px; background: var(--ink); opacity: 0.55; border-radius: 1px; }
  .spark { width: 100%; height: 44px; }
  .even { stroke: var(--line-2); stroke-dasharray: 3 3; }
  .area { fill: var(--accent-soft); }
  .path { fill: none; stroke: var(--accent); stroke-width: 1.5; vector-effect: non-scaling-stroke; }
  .note { font-size: 11px; color: var(--ink-3); }
  .crit { color: var(--critical); }
  .warnline { color: var(--warning); }
</style>
