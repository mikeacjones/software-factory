<script lang="ts">
  import { live, route, go } from './lib/live.svelte';
  import Sidebar from './components/Sidebar.svelte';
  import Inbox from './components/Inbox.svelte';
  import Floor from './components/Floor.svelte';
  import ProjectView from './components/ProjectView.svelte';
  import RunDrawer from './components/RunDrawer.svelte';

  const project = $derived(
    route.at.view === 'project' ? (live.picture?.projects || []).find((one) => one.id === (route.at as { project: string }).project) : undefined,
  );

  // The tab's title says how much is waiting on the owner.
  $effect(() => {
    const waiting = (live.picture?.inbox || []).filter((item) => !live.sent[item.key]).length;
    document.title = waiting ? `(${waiting}) Factory` : 'Factory';
  });

  // g then a key goes somewhere: i the inbox, f the floor, 1-9 a project.
  let leader = 0;
  function keys(event: KeyboardEvent) {
    const typing = event.target instanceof HTMLElement && /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName);
    if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === 'g') {
      leader = Date.now();
      return;
    }
    if (Date.now() - leader > 1200) return;
    leader = 0;
    if (event.key === 'i') go({ view: 'inbox' });
    else if (event.key === 'f') go({ view: 'floor' });
    else if (/^[1-9]$/.test(event.key)) {
      const one = (live.picture?.projects || [])[Number(event.key) - 1];
      if (one) go({ view: 'project', project: one.id });
    } else return;
    event.preventDefault();
    event.stopImmediatePropagation();
  }
</script>

<svelte:window onkeydowncapture={keys} />

<div class="shell">
  <Sidebar />
  <main>
    {#if !live.picture}
      <p class="loading quiet">Reading the factory's records…</p>
    {:else if route.at.view === 'floor'}
      <Floor />
    {:else if route.at.view === 'project'}
      {#if project}
        <ProjectView {project} node={route.at.node} />
      {:else}
        <p class="loading quiet">The factory has no project called {route.at.project}.</p>
      {/if}
    {:else}
      <Inbox />
    {/if}
  </main>
</div>
<RunDrawer />

<style>
  .shell { display: grid; grid-template-columns: 236px 1fr; height: 100%; }
  main { min-width: 0; min-height: 0; height: 100%; overflow: hidden; }
  .loading { padding: 40px; }
  @media (max-width: 700px) {
    .shell { grid-template-columns: 1fr; grid-template-rows: auto 1fr; }
    .shell :global(.side) { height: auto; max-height: 34vh; }
  }
</style>
