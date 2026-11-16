'use strict';

// The page asks the server for the picture of the factory every two
// seconds and redraws only what changed. Times are kept as times and
// turned into "for 4 min" here, once a second, so the picture itself
// changes only when the factory's records do.

const el = (id) => document.getElementById(id);
const esc = (text) => String(text ?? '').replace(/[&<>"']/g, (c) => (
  { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
));

let skew = 0;            // the server's clock minus this one's, in seconds
let tag = null;          // the picture last drawn
let heard = 0;           // when the server last answered, by this clock
let release = '';
const runs = new Map();      // what each Watch button opens
const activity = new Map();  // when each run's log last grew
const trails = new Map();    // what each small chart draws
const opened = new Map();    // which folds the reader opened or closed
const drawn = new Map();     // what each part of the page shows now

const now = () => Date.now() / 1000 + skew;

function span(secs) {
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs} s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min`;
  const hours = Math.floor(mins / 60);
  if (hours < 48) return `${hours} h${mins % 60 ? ` ${mins % 60} min` : ''}`;
  return `${Math.floor(hours / 24)} d${hours % 24 ? ` ${hours % 24} h` : ''}`;
}

function clock(at) {
  const day = new Date(at * 1000);
  const today = new Date(now() * 1000).toDateString() === day.toDateString();
  const time = day.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  return today ? time : `${day.toLocaleDateString(undefined, { weekday: 'short' })} ${time}`;
}

const pct = (share) => `${Math.round(share * 100)}%`;
const count = (n, thing) => `${Number(n) || 0} ${thing}${Number(n) === 1 ? '' : 's'}`;
const money = (usd) => {
  usd = Math.max(0, usd || 0);
  return `$${usd >= 100 ? Math.round(usd).toLocaleString() : usd.toFixed(2)}`;
};
const time = (at, mode) => `<time class="num" data-at="${Number(at)}" data-mode="${mode}"></time>`;
const model = (name) => String(name || '').replace(/^claude-/, '');
const short = (bead, project) => (
  bead && project && bead.startsWith(`${project}-`) ? bead.slice(project.length + 1) : (bead || '')
);
const link = (url, text) => (
  url ? `<a class="link" href="${esc(url)}" target="_blank" rel="noopener noreferrer">${esc(text)} ↗</a>` : ''
);
const pill = (tone, text, title) => (
  `<span class="pill ${tone}"${title ? ` title="${esc(title)}"` : ''}>${esc(text)}</span>`
);

// A state of a goal, a sub-goal or a bead: the mark it wears and the
// word beside it.
function state(word) {
  const tones = {
    running: 'running', shipping: 'running', working: 'running',
    absorbed: 'done', landed: 'done', complete: 'done', ended: 'done', merged: 'done',
    planned: 'todo', open: 'todo', queued: 'todo',
    parked: 'wait', waiting: 'wait', proposed: 'wait', proposed_complete: 'wait', held: 'wait',
    failed: 'bad', refused: 'bad', dropped: 'bad', abandoned: 'bad',
  };
  const text = String(word || 'unknown').replace(/_/g, ' ');
  return [tones[String(word || '').toLowerCase()] || '', text.charAt(0).toUpperCase() + text.slice(1)];
}

function remember(run, files, title, sub) {
  const key = `${run.where}/${run.id}/${run.file}`;
  runs.set(key, { where: run.where, id: run.id, file: run.file, files: files || [run.file], title, sub });
  if (run.active_at) activity.set(key, run.active_at);
  return key;
}
const watch = (key, text) => `<button class="btn" type="button" data-run="${esc(key)}">${text}</button>`;

// What an agent's run is, in words: who, at what, on which account.
function runTitle(run) {
  return run.step ? `${run.role} · ${run.step}` : `${run.role}`;
}

function fold(key, open) {
  return `data-key="${esc(key)}"${(opened.has(key) ? opened.get(key) : open) ? ' open' : ''}`;
}

// ---- Running now ----------------------------------------------------

function nowHtml(picture) {
  const rows = [];
  for (const run of picture.runs || []) {
    const files = filesOf(picture, run);
    const what = run.where === 'unit'
      ? (run.title || short(run.bead, run.project))
      : (run.where === 'goal' ? `Goal ${run.goal}` : `Project ${run.project}`);
    const id = run.where === 'unit' ? short(run.bead, run.project) : run.project;
    const key = remember(run, files, runTitle(run), what);
    const account = run.account
      ? `account <b>${esc(run.account)}</b>`
      : 'account on record when it ends';
    rows.push(`<div class="run-row">
      <div class="run-who">
        <span class="pill running"><strong>${esc(run.role)}</strong>${run.step ? ` · ${esc(run.step)}` : ''}</span>
        <span class="run-meta">${esc(model(run.model))}${run.effort ? ` · ${esc(run.effort)}` : ''} · ${account}</span>
      </div>
      <div class="run-what">
        <span class="title">${esc(what)}</span>
        <span class="run-meta mono">${esc(id)}</span>
      </div>
      <div class="run-end">
        <span class="run-meta">for ${time(run.since, 'for')} · <span data-quiet="${esc(key)}"></span></span>
        ${watch(key, 'Watch')}
      </div>
    </div>`);
  }
  for (const bead of beadsOf(picture)) {
    if (!bead.command) continue;
    rows.push(`<div class="run-row">
      <div class="run-who">
        <span class="pill running"><strong>the factory</strong> · ${esc(bead.command.step)}</span>
        <span class="run-meta">a check of its own, no agent · round ${Number(bead.command.round) || 1}</span>
      </div>
      <div class="run-what">
        <span class="title">${esc(bead.title || bead.bead)}</span>
        <span class="run-meta mono">${esc(short(bead.bead, bead.project))}</span>
      </div>
      <div class="run-end"><span class="run-meta">for ${time(bead.command.since, 'for')}</span></div>
    </div>`);
  }
  if (!rows.length) {
    return '<p class="quiet">No agent is running. The tiers wake when a pipeline ends, when the owner writes, or when a time comes due.</p>';
  }
  return rows.join('');
}

function* beadsOf(picture) {
  for (const project of picture.projects || []) {
    for (const goal of project.goals || []) {
      for (const sub of goal.subgoals || []) {
        for (const bead of sub.beads || []) yield { ...bead, project: project.id };
        if (sub.ship) yield { ...sub.ship, project: project.id };
      }
    }
    for (const bead of project.loose || []) yield { ...bead, project: project.id };
  }
}

function filesOf(picture, run) {
  for (const project of picture.projects || []) {
    if (run.where === 'project' && project.id === run.id) return project.executive?.runs;
    for (const goal of project.goals || []) {
      if (run.where === 'goal' && goal.id === run.id) return goal.runs;
    }
  }
  for (const bead of beadsOf(picture)) {
    if (run.where === 'unit' && bead.unit === run.id) return bead.runs;
  }
  return [run.file];
}

// ---- Accounts -------------------------------------------------------

function windowHtml(account, win, label, perWhat) {
  const used = win.utilization || 0;
  const level = used >= 0.9 ? 'crit' : used >= 0.7 ? 'warn' : '';
  const open = win.resets_at != null;
  const start = open ? win.resets_at - win.length : 0;
  const notes = [];
  if (open) {
    notes.push(`Resets in ${time(win.resets_at, 'in')}, at ${esc(clock(win.resets_at))}.`);
  } else {
    notes.push('No window is open. The next run on this account opens one.');
  }
  const rate = win.per_hour == null ? null : win.per_hour * (perWhat === 'day' ? 24 : 1);
  if (open && rate == null) notes.push('Too few readings yet to say how fast it is going.');
  if (open && rate === 0) notes.push('Nothing used lately.');
  if (open && rate > 0) notes.push(`Using <b>${pct(rate)}</b> a${perWhat === 'day' ? ' day' : 'n hour'} lately.`);
  let warn = '';
  if (win.status && win.status !== 'allowed') {
    warn = `<p class="warnline crit">At its limit (${esc(String(win.status).replace(/_/g, ' '))})${open ? ` until ${esc(clock(win.resets_at))}` : ''}.</p>`;
  } else if (win.full_at) {
    warn = `<p class="warnline">At this rate it is full at ${esc(clock(win.full_at))}, ${esc(span(win.resets_at - win.full_at))} before it resets.</p>`;
  } else if (open && rate > 0) {
    notes.push('At this rate it lasts until it resets.');
  }
  const pace = open
    ? `<div class="pace" data-pace-start="${start}" data-pace-length="${win.length}" title="How much of the window's time has gone. A bar past this mark is ahead of an even pace."></div>`
    : '';
  return `<div class="window">
    <div class="window-top"><span class="label">${label}</span><span class="value">${pct(used)}</span></div>
    <div class="meter ${level}${used ? '' : ' empty'}" role="meter" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${Math.round(used * 100)}" aria-label="${esc(account)}: ${label} used">
      <div class="fill" data-w="${Math.min(100, used * 100)}"></div>${pace}
    </div>
    <p class="window-note">${notes.join(' ')}</p>
    ${warn}
    ${open ? spark(`${account}/${label}`, win, start) : ''}
  </div>`;
}

// The window so far as a small chart: time across, share used upward,
// both drawn to the whole window so that two accounts compare by eye.
const W = 320, H = 56, TOP = 6, BASE = 52;
function spark(key, win, start) {
  const points = (win.trail || []).filter(([at]) => at >= start && at <= win.resets_at);
  if (points.length < 2) return '';
  trails.set(key, { points, start, length: win.length });
  const x = (at) => ((at - start) / win.length) * W;
  const y = (used) => BASE - Math.min(1, Math.max(0, used)) * (BASE - TOP);
  const line = points.map(([at, used]) => `${x(at).toFixed(1)},${y(used).toFixed(1)}`).join(' ');
  const [lastAt, lastUsed] = points[points.length - 1];
  let ahead = '';
  if (win.per_hour > 0) {
    // Where it stands when it is full, or when it resets, at this rate.
    const until = Math.min(win.full_at || Infinity, win.resets_at);
    const reach = win.full_at ? 1 : Math.min(1, lastUsed + (win.per_hour * Math.max(0, until - now())) / 3600);
    ahead = `<line class="ahead" x1="${x(lastAt).toFixed(1)}" y1="${y(lastUsed).toFixed(1)}" x2="${x(until).toFixed(1)}" y2="${y(reach).toFixed(1)}"/>`;
  }
  const dot = `x1="${x(lastAt).toFixed(1)}" y1="${y(lastUsed).toFixed(1)}" x2="${x(lastAt).toFixed(1)}" y2="${y(lastUsed).toFixed(1)}"`;
  return `<svg class="spark" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" data-spark="${esc(key)}" role="img" aria-label="Share of the window used over its time so far, with an even pace drawn for comparison">
    <line class="grid" x1="0" y1="${BASE}" x2="${W}" y2="${BASE}"/>
    <line class="grid" x1="0" y1="${TOP}" x2="${W}" y2="${TOP}"/>
    <line class="even" x1="0" y1="${BASE}" x2="${W}" y2="${TOP}"/>
    <polygon class="area" points="${x(points[0][0]).toFixed(1)},${BASE} ${line} ${x(lastAt).toFixed(1)},${BASE}"/>
    <polyline class="path" points="${line}"/>
    ${ahead}
    <line class="cross" x1="0" y1="${TOP}" x2="0" y2="${BASE}" visibility="hidden"/>
    <line class="end-ring" ${dot}/><line class="end" ${dot}/>
    <rect x="0" y="0" width="${W}" height="${H}" fill="transparent"/>
  </svg>
  <div class="spark-axis"><span>opened ${esc(clock(start))}</span><span>dashed: an even pace</span><span>resets ${esc(clock(win.resets_at))}</span></div>`;
}

function accountHtml(account) {
  const working = (account.runs || []).length;
  const blocked = Object.keys(account.blocked || {});
  const limited = [account.five_hour, account.seven_day].some((win) => win.status && win.status !== 'allowed');
  let status = pill('todo', 'Idle');
  if (working) status = pill('running', `Working: ${working} run${working > 1 ? 's' : ''}`);
  if (blocked.length) status = pill('wait', `Blocked for ${blocked.join(', ')}`);
  if (limited) status = pill('bad', 'At its limit');
  const on = (account.runs || []).map((run) => (
    `<b>${esc(run.role)}</b>${run.step ? ` · ${esc(run.step)}` : ''} on <span class="mono">${esc(short(run.bead, run.project) || run.id)}</span>`
  ));
  const foot = [
    working ? `Work is on it now: ${on.join('; ')}.` : (
      account.handed_at ? `Last given work ${time(account.handed_at, 'ago')}.` : 'Given no work lately.'
    ),
    `Runs that ended cost <b>${money(account.spent_hour)}</b> in the last hour and <b>${money(account.spent_day)}</b> in the last day, at list prices.`,
  ];
  if (account.concern) foot.unshift(`<span class="warnline">${esc(account.concern)}</span>`);
  return `<article class="account">
    <div class="account-top"><h3>${esc(account.name)}</h3>${status}</div>
    ${windowHtml(account.name, account.five_hour, '5-hour window', 'hour')}
    ${windowHtml(account.name, account.seven_day, '7-day window', 'day')}
    <div class="account-foot">${foot.map((line) => `<p>${line}</p>`).join('')}</div>
  </article>`;
}

// ---- Projects, goals, sub-goals, beads -------------------------------

function trackHtml(bead) {
  const steps = bead.steps || [];
  if (!steps.length) return '';
  const here = bead.run?.step || bead.command?.step || bead.step;
  const going = bead.status === 'running';
  const over = bead.status === 'ended';
  return `<div class="track" role="img" aria-label="The pipeline's steps: ${esc(steps.filter((s) => s.rounds).length)} of ${steps.length} reached">${steps.map((step) => {
    let tone = step.rounds ? `done${step.rounds > 1 ? ' again' : ''}` : '';
    if (step.id === here && !over) tone = going ? 'now' : 'stopped';
    const said = `${step.id}${step.kind === 'agent' && step.role ? ` (${step.role})` : ''}${step.rounds > 1 ? `, ${step.rounds} times` : ''}`;
    return `<i class="st ${tone}" title="${esc(said)}"></i>`;
  }).join('')}</div>`;
}

function whereHtml(bead) {
  if (bead.run) {
    const run = bead.run;
    const round = (bead.steps || []).find((step) => step.id === run.step)?.rounds || 0;
    return `<b>${esc(run.step)}</b>${round > 1 ? ` · round ${round}` : ''} · ${esc(run.role)} on ${esc(model(run.model))} · ${esc(run.account)} · ${time(run.since, 'for')}`;
  }
  if (bead.command) {
    return `<b>${esc(bead.command.step)}</b> · the factory's own check${bead.command.round > 1 ? ` · round ${Number(bead.command.round)}` : ''} · ${time(bead.command.since, 'for')}`;
  }
  if (!bead.unit) return '';
  if (bead.status === 'running') return `<b>${esc(bead.step)}</b> · between steps`;
  if (bead.status === 'ended') return `Ended at ${esc(bead.step)} ${time(bead.changed_at, 'ago')}`;
  return `${esc(state(bead.status)[1])} at <b>${esc(bead.step)}</b> since ${time(bead.changed_at, 'ago')}`;
}

function beadHtml(bead, project, ship) {
  const id = short(bead.bead, project);
  let [tone, word] = state(bead.state);
  if (bead.status && !['running', 'ended'].includes(bead.status) && bead.state === 'running') {
    [tone, word] = state(bead.status);
  }
  let button = '';
  if (bead.run) {
    button = watch(remember(bead.run, bead.runs, runTitle(bead.run), bead.title || id), 'Watch');
  } else if (bead.runs?.length) {
    const last = { where: 'unit', id: bead.unit, file: bead.runs[bead.runs.length - 1] };
    button = watch(remember(last, bead.runs, 'Runs of this bead', bead.title || id), 'Runs');
  }
  const request = bead.pull_request ? link(bead.pull_request, `#${String(bead.pull_request).split('/').pop()}`) : '';
  return `<div class="bead${ship ? ' ship' : ''}">
    <div class="bead-name">
      <span class="title" title="${esc(bead.title || '')}">${esc(bead.title || id)}</span>
      <span class="id">${bead.title ? esc(id) : (bead.unit ? esc(id) : 'not started')}</span>
    </div>
    ${pill(tone, word, bead.detail)}
    <div class="bead-line">${trackHtml(bead)}<span class="where">${whereHtml(bead)}</span></div>
    <div class="bead-end">
      ${bead.cost ? `<span class="quiet num" title="What its runs cost so far, at list prices">${money(bead.cost)}</span>` : ''}
      ${request}${link(bead.workflow, 'Workflow')}${button}
    </div>
  </div>`;
}

function subHtml(goal, sub, project) {
  const beads = sub.beads || [];
  const done = beads.filter((bead) => ['absorbed', 'landed'].includes(bead.state)).length;
  const [tone, word] = state(sub.state);
  const foreman = sub.foreman || {};
  const said = [count(foreman.wakes, 'wake')];
  if (foreman.waiting) said.push(`reports <b>${esc(foreman.waiting)}</b>`);
  if (foreman.inbox) said.push(`${foreman.inbox} unread`);
  if (foreman.failures) said.push(`<b>${count(foreman.failures, 'failure')}</b>`);
  if (foreman.retry_at) said.push(`tried again at ${esc(clock(foreman.retry_at))}`);
  const key = `sub:${goal.id}/${sub.id}`;
  return `<details class="sub" ${fold(key, !['landed', 'dropped'].includes(sub.state))}>
    <summary>
      <div class="sub-name">
        <span class="title">${esc(sub.title || sub.id)}</span>
        <span class="quiet">${done} of ${beads.length} beads absorbed${sub.branch ? ` · <span class="mono">${esc(sub.branch)}</span>` : ''}</span>
      </div>
      <div class="sub-side">${pill(tone, word)}${sub.pull_request ? link(sub.pull_request, `Pull request #${String(sub.pull_request).split('/').pop()}`) : ''}</div>
    </summary>
    <div class="sub-body">
      <p class="tier"><span class="who">Foreman</span><span>${said.join(' · ')}</span></p>
      ${foreman.message ? `<details class="more" ${fold(`${key}/said`, false)}><summary>What the foreman last reported</summary><p class="objective">${esc(foreman.message)}</p></details>` : ''}
      ${sub.objective ? `<details class="more" ${fold(`${key}/for`, false)}><summary>What this sub-goal is for</summary><p class="objective">${esc(sub.objective)}</p></details>` : ''}
      <div class="beads">
        ${beads.map((bead) => beadHtml(bead, project, false)).join('')}
        ${sub.ship ? beadHtml(sub.ship, project, true) : ''}
      </div>
    </div>
  </details>`;
}

function tierRun(run, files, title, sub, asleep) {
  if (run) {
    const key = remember(run, files, runTitle(run), sub);
    return `${pill('running', `${run.role} at work`)} <span>for ${time(run.since, 'for')}</span> ${watch(key, 'Watch')}`;
  }
  if (!files?.length) return pill('todo', asleep);
  const last = { where: title.where, id: title.id, file: files[files.length - 1] };
  return `${pill('todo', asleep)} ${watch(remember(last, files, title.text, sub), 'Past runs')}`;
}

function goalHtml(goal, project) {
  const [tone, word] = state(goal.phase);
  const said = [count(goal.wakes, 'wake')];
  if (goal.inbox) said.push(`${goal.inbox} unread`);
  if (goal.failures) said.push(`<b>${count(goal.failures, 'failure')}</b>`);
  if (goal.retry_at) said.push(`tries again at ${esc(clock(goal.retry_at))}`);
  return `<article class="goal">
    <div class="goal-top">
      <p class="tier">${pill(tone, word)}<span class="mono faint">${esc(goal.id)}</span><span>started ${time(goal.started_at, 'ago')}</span><span title="What its directors' and foremen's runs cost, at list prices">${money(goal.cost)} on its tiers</span></p>
      <p class="objective">${esc(goal.objective)}</p>
      ${goal.asked ? `<div class="callout"><b>Waiting for the owner</b>${esc(goal.asked)}</div>` : ''}
      <p class="tier"><span class="who">Director and foremen</span>${tierRun(goal.run, goal.runs, { where: 'goal', id: goal.id, text: 'Runs of the goal\'s tiers' }, `Goal ${goal.id}`, 'Asleep until something happens')}<span>${said.join(' · ')}</span></p>
      ${goal.stop_when ? `<details class="more" ${fold(`goal:${goal.id}/stop`, false)}><summary>When this goal is done</summary><p class="objective">${esc(goal.stop_when)}</p></details>` : ''}
    </div>
    ${(goal.subgoals || []).map((sub) => subHtml(goal, sub, project)).join('')}
  </article>`;
}

function projectHtml(project) {
  const exec = project.executive;
  let executive = '';
  if (exec) {
    const pending = exec.pending || [];
    const said = [`${exec.threads} thread${exec.threads === 1 ? '' : 's'} with the owner`];
    if (exec.waiting) said.push(`<b>${exec.waiting} waiting for an account</b>`);
    if (exec.memos) said.push(`${exec.memos} memo${exec.memos === 1 ? '' : 's'} to read`);
    said.push(count(exec.wakes, 'wake'), money(exec.cost));
    executive = `<p class="tier"><span class="who">Executive</span>${tierRun(exec.run, exec.runs, { where: 'project', id: project.id, text: 'Runs of the executive' }, `Project ${project.id}`, 'Asleep until the owner writes')}<span>${said.join(' · ')}</span></p>
      ${pending.length ? `<details class="more" ${fold(`exec:${project.id}/pending`, false)}><summary>${pending.length} bead${pending.length === 1 ? '' : 's'} to close once another has closed</summary><ul class="finished">${pending.map((one) => `<li><span><span class="mono">${esc(short(one.bead, project.id))}</span> as ${esc(one.as)}, once <span class="mono">${esc(short(one.once_closed, project.id))}</span> is closed</span></li>`).join('')}</ul></details>` : ''}`;
  }
  const goals = (project.goals || []).map((goal) => goalHtml(goal, project.id)).join('');
  const loose = (project.loose || []).length
    ? `<article class="goal"><div class="goal-top"><p class="tier"><span class="who">Units run by hand, in no goal</span></p><div class="beads">${project.loose.map((bead) => beadHtml(bead, project.id, false)).join('')}</div></div></article>`
    : '';
  const finished = (project.finished || []).length
    ? `<details class="more" ${fold(`done:${project.id}`, false)}><summary>${project.finished.length} finished goal${project.finished.length === 1 ? '' : 's'}</summary><ul class="finished">${project.finished.map((goal) => `<li><span>${esc(goal.objective || goal.id)}</span><span class="num">${money(goal.cost)}</span></li>`).join('')}</ul></details>`
    : '';
  return `<section class="project" aria-label="Project ${esc(project.id)}">
    <div class="project-top"><h2>${esc(project.id)}</h2>${link(project.workflow, 'Project workflow')}</div>
    ${executive}
    ${goals || (loose ? '' : '<p class="quiet">No goal is open.</p>')}
    ${loose}
    ${finished}
  </section>`;
}

// ---- Drawing ----------------------------------------------------------

function patch(node, html) {
  if (drawn.get(node) === html) return;
  drawn.set(node, html);
  node.innerHTML = html;
  for (const bar of node.querySelectorAll('[data-w]')) bar.style.width = `${bar.dataset.w}%`;
}

function draw(picture) {
  runs.clear();
  activity.clear();
  trails.clear();
  release = picture.release || '';
  patch(el('now'), nowHtml(picture));
  patch(el('accounts'), (picture.accounts || []).map(accountHtml).join('')
    || '<p class="quiet">The factory has no record of its accounts yet.</p>');
  patch(el('projects'), (picture.projects || []).map(projectHtml).join(''));
  tick();
}

// Once a second: every "for 4 min", every pace mark, and the word on
// whether what is shown is fresh.
function tick() {
  const at = now();
  for (const node of document.querySelectorAll('time[data-at]')) {
    const then = Number(node.dataset.at);
    const text = node.dataset.mode === 'in' ? span(then - at)
      : node.dataset.mode === 'ago' ? `${span(at - then)} ago` : span(at - then);
    if (node.textContent !== text) node.textContent = text;
  }
  for (const node of document.querySelectorAll('[data-quiet]')) {
    const last = activity.get(node.dataset.quiet);
    const quiet = last ? at - last : 0;
    const text = !last ? '' : quiet < 30 ? 'writing now' : `quiet for ${span(quiet)}`;
    if (node.textContent !== text) node.textContent = text;
  }
  for (const node of document.querySelectorAll('[data-pace-start]')) {
    const gone = (at - Number(node.dataset.paceStart)) / Number(node.dataset.paceLength);
    node.style.left = `${Math.min(100, Math.max(0, gone * 100)).toFixed(2)}%`;
  }
  const pulse = el('pulse');
  if (!heard) return;
  const silent = Date.now() / 1000 - heard;
  const stale = silent > 12;
  pulse.classList.toggle('stale', stale);
  const text = stale
    ? `Not updated for ${span(silent)}: the dashboard's server is not answering`
    : `Live · read from the factory's records every 2 s${release ? ` · release ${release}` : ''}`;
  if (pulse.textContent !== text) pulse.textContent = text;
}

async function poll() {
  try {
    const answer = await fetch('/api/snapshot', {
      cache: 'no-store',
      headers: tag ? { 'If-None-Match': tag } : {},
    });
    const serverNow = Number(answer.headers.get('x-now'));
    if (serverNow) skew = serverNow - Date.now() / 1000;
    if (answer.status === 200) {
      const picture = await answer.json();
      tag = answer.headers.get('etag');
      draw(picture);
    }
    if (answer.status === 200 || answer.status === 304) heard = Date.now() / 1000;
  } catch {
    // The next look says so if it goes on.
  }
  setTimeout(poll, document.hidden ? 15000 : 2000);
}

// ---- A run, read as it happens -----------------------------------------

let watching = null;
let opener = null;

function openRun(key, file) {
  const run = runs.get(key) || (watching && watching.key === key ? watching.run : null);
  if (!run) return;
  if (watching) clearTimeout(watching.timer);
  watching = { key, run, file: file || run.file, next: null, ended: false, timer: 0 };
  el('drawer-title').textContent = run.title || 'Run';
  el('drawer-sub').textContent = run.sub || '';
  const select = el('drawer-run');
  select.textContent = '';
  for (const name of [...(run.files || [])].reverse()) {
    const option = document.createElement('option');
    option.value = name;
    option.textContent = name.replace(/\.jsonl$/, '');
    option.selected = name === watching.file;
    select.append(option);
  }
  el('feed').textContent = '';
  el('drawer-state').textContent = 'Reading…';
  el('drawer').hidden = false;
  el('drawer-close').focus();
  pump();
}

function closeRun() {
  if (watching) clearTimeout(watching.timer);
  watching = null;
  el('drawer').hidden = true;
  if (opener?.isConnected) opener.focus();
}

function say(tone, build) {
  const item = document.createElement('li');
  item.className = tone;
  build(item);
  return item;
}

function entry(event) {
  switch (event.k) {
    case 'say':
      return say('say', (item) => { item.textContent = event.text; });
    case 'tool':
      return say('tool', (item) => {
        const name = document.createElement('b');
        name.textContent = event.name;
        const asked = document.createElement('code');
        asked.textContent = event.text;
        item.append(name, asked);
      });
    case 'out':
      if (!event.text) return null;
      return say(`out${event.err ? ' err' : ''}`, (item) => {
        const text = document.createElement('pre');
        text.textContent = event.text;
        text.title = 'Click to show all of it';
        item.append(text);
      });
    case 'start':
      return say('mark', (item) => { item.textContent = `Began: ${event.text}`; });
    case 'end':
      return say('mark', (item) => {
        const head = document.createElement('b');
        const cost = event.cost != null ? ` · ${money(event.cost)}` : '';
        const turns = event.turns != null ? ` · ${event.turns} turns` : '';
        head.textContent = `Ended (${event.outcome || 'no outcome said'})${turns}${cost}`;
        item.append(head, event.text ? `\n${event.text}` : '');
      });
    default:
      return null;
  }
}

async function pump() {
  const mine = watching;
  if (!mine) return;
  let more = false;
  try {
    const query = new URLSearchParams({ where: mine.run.where, id: mine.run.id, file: mine.file });
    if (mine.next != null) query.set('from', mine.next);
    const answer = await fetch(`/api/run?${query}`, { cache: 'no-store' });
    if (mine !== watching) return;
    if (!answer.ok) {
      el('drawer-state').textContent = 'This run\'s log cannot be read.';
    } else {
      const part = await answer.json();
      if (mine !== watching) return;
      const feed = el('feed');
      const following = feed.scrollTop + feed.clientHeight >= feed.scrollHeight - 60;
      if (mine.next == null && part.from > 0) {
        feed.append(say('mark', (item) => {
          item.textContent = `The first ${Math.round(part.from / 1024)} KB of this run are not shown.`;
        }));
      }
      for (const event of part.events) {
        const item = entry(event);
        if (item) feed.append(item);
        if (event.k === 'end') mine.ended = true;
      }
      while (feed.children.length > 2000) feed.firstElementChild.remove();
      if (following) feed.scrollTop = feed.scrollHeight;
      mine.next = part.next;
      more = part.next < part.size && part.events.length > 0;
      el('drawer-state').textContent = mine.ended
        ? 'This run has ended.'
        : 'Following: new lines appear as the agent writes them.';
    }
  } catch {
    el('drawer-state').textContent = 'The dashboard\'s server is not answering.';
  }
  if (mine === watching && !mine.ended) {
    mine.timer = setTimeout(pump, more ? 0 : document.hidden ? 6000 : 1500);
  }
}

// ---- What the reader does ----------------------------------------------

document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-run]');
  if (button) {
    opener = button;
    openRun(button.dataset.run);
    return;
  }
  const out = event.target.closest('.feed .out');
  if (out) out.classList.toggle('open');
});
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && watching) closeRun();
});
document.addEventListener('toggle', (event) => {
  const key = event.target.dataset?.key;
  if (key) opened.set(key, event.target.open);
}, true);

// The small charts answer the pointer: the reading nearest to it.
function pointAt(event) {
  const chart = event.target.closest?.('svg[data-spark]');
  const tip = el('tip');
  if (!chart) {
    tip.hidden = true;
    for (const cross of document.querySelectorAll('.spark .cross')) cross.setAttribute('visibility', 'hidden');
    return;
  }
  const trail = trails.get(chart.dataset.spark);
  if (!trail) return;
  const box = chart.getBoundingClientRect();
  const at = trail.start + ((event.clientX - box.left) / box.width) * trail.length;
  let near = trail.points[0];
  for (const point of trail.points) {
    if (Math.abs(point[0] - at) < Math.abs(near[0] - at)) near = point;
  }
  const x = ((near[0] - trail.start) / trail.length) * W;
  const cross = chart.querySelector('.cross');
  cross.setAttribute('x1', x);
  cross.setAttribute('x2', x);
  cross.setAttribute('visibility', 'visible');
  tip.textContent = `${clock(near[0])} · ${pct(near[1])} used`;
  tip.hidden = false;
  const left = Math.min(window.innerWidth - tip.offsetWidth - 8, Math.max(8, event.clientX + 12));
  tip.style.left = `${left}px`;
  tip.style.top = `${Math.max(8, event.clientY - 34)}px`;
}
document.addEventListener('pointermove', pointAt);
document.addEventListener('pointerdown', pointAt);

function begin() {
  el('drawer-close').addEventListener('click', closeRun);
  el('drawer-run').addEventListener('change', (event) => {
    if (watching) openRun(watching.key, event.target.value);
  });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) tick(); });
  setInterval(tick, 1000);
  poll();
}
begin();
