// The live state of the page: the picture, kept up to date by the server
// as it changes, the server's clock, where the reader is, and what the
// owner has sent that the factory has not yet taken in.

import type { Item, Picture, Line } from './types';

export const live = $state({
  picture: null as Picture | null,
  /** When the server last answered, by this machine's clock. */
  heard: 0,
  /** The server's clock minus this one's, in seconds. */
  skew: 0,
  /** The server's time now, ticking once a second. */
  now: Date.now() / 1000,
  canAct: false,
  whyNot: '' as string,
  /** Acts sent, by inbox item, until the item leaves the inbox. */
  sent: {} as Record<string, { label: string; at: number }>,
  /** New messages to a project, until its records show the conversation. */
  started: {} as Record<string, { text: string; at: number; thread: string }[]>,
});

export type View =
  | { view: 'inbox'; item?: string }
  | { view: 'floor' }
  | { view: 'project'; project: string; node?: string };

function read(hash: string): View {
  const parts = hash.replace(/^#\/?/, '').split('/').map(decodeURIComponent);
  if (parts[0] === 'floor') return { view: 'floor' };
  if (parts[0] === 'p' && parts[1]) return { view: 'project', project: parts[1], node: parts[2] || undefined };
  return { view: 'inbox', item: parts[0] === 'inbox' && parts[1] ? parts[1] : undefined };
}

export const route = $state({ at: read(location.hash) });

export function go(view: View) {
  const hash = view.view === 'floor' ? '#/floor'
    : view.view === 'project' ? `#/p/${encodeURIComponent(view.project)}${view.node ? `/${encodeURIComponent(view.node)}` : ''}`
      : `#/inbox${view.item ? `/${encodeURIComponent(view.item)}` : ''}`;
  if (location.hash !== hash) history.pushState(null, '', hash);
  route.at = view;
}

addEventListener('popstate', () => { route.at = read(location.hash); });
addEventListener('hashchange', () => { route.at = read(location.hash); });

// The picture comes as a stream: the whole of it once, then only what
// changed in it, applied where it is so that everything not touched by a
// change (what is being read, typed or dragged) is left as it was.

type Change = ['s', (string | number)[], unknown] | ['d', (string | number)[]] | ['t', (string | number)[], number];

let tag: string | null = null;

/** Apply the server's changes to the picture held (crates/factory-dash/src/patch.rs).
 * False if one does not fit, and then the picture must be read whole again. */
function apply(changes: Change[]): boolean {
  for (const change of changes) {
    const path = change[1];
    if (!path.length) {
      if (change[0] !== 's') return false;
      live.picture = change[2] as Picture;
      continue;
    }
    let at: any = live.picture;
    for (const step of path.slice(0, -1)) {
      at = at?.[step];
      if (at == null || typeof at !== 'object') return false;
    }
    const last = path[path.length - 1];
    if (change[0] === 's') {
      if (Array.isArray(at) && (typeof last !== 'number' || last > at.length)) return false;
      at[last] = change[2];
    } else if (change[0] === 'd') {
      delete at[last];
    } else {
      const list = at[last];
      if (!Array.isArray(list)) return false;
      list.length = change[2];
    }
  }
  return true;
}

/** What the owner sent and the records now show has been taken in. */
function settle(picture: Picture) {
  const waiting = new Set((picture.inbox || []).map((item) => item.key));
  for (const key of Object.keys(live.sent)) if (!waiting.has(key)) delete live.sent[key];
  // A new message is shown as sent until its thread is on record.
  for (const project of picture.projects || []) {
    const known = new Set([
      ...(project.executive?.conversations || []).map((one) => one.ts),
      ...(project.goals || []).map((goal) => goal.thread?.ts),
    ]);
    const left = (live.started[project.id] || []).filter((one) => !known.has(one.thread));
    if (left.length !== (live.started[project.id] || []).length) live.started[project.id] = left;
  }
}

function heard(now: unknown) {
  if (typeof now === 'number' && now) live.skew = now - Date.now() / 1000;
  live.heard = Date.now() / 1000;
}

let source: EventSource | null = null;

function listen() {
  source?.close();
  const stream = new EventSource('/api/stream');
  source = stream;
  stream.addEventListener('picture', (event) => {
    const said = JSON.parse((event as MessageEvent).data);
    tag = said.tag;
    live.picture = said.picture;
    settle(live.picture!);
    heard(said.now);
  });
  stream.addEventListener('patch', (event) => {
    const said = JSON.parse((event as MessageEvent).data);
    // A change made to some other picture than this one: start again.
    if (said.from !== tag || !apply(said.changes)) {
      listen();
      return;
    }
    tag = said.tag;
    settle(live.picture!);
    heard(said.now);
  });
  stream.addEventListener('beat', (event) => heard(JSON.parse((event as MessageEvent).data).now));
  // The browser asks again by itself when the stream is lost; the pulse
  // says so if it goes on.
}

async function askCanAct() {
  try {
    const can = await (await fetch('/api/can-act', { cache: 'no-store' })).json();
    live.canAct = !!can.act;
    live.whyNot = can.why || '';
  } catch {
    live.canAct = false;
    live.whyNot = 'The dashboard\'s server is not answering.';
  }
}

export function start() {
  listen();
  askCanAct();
  setInterval(() => { live.now = Date.now() / 1000 + live.skew; }, 1000);
}

export type Sent = { ok: true; said: string } | { ok: false; error: string };

async function send(body: Record<string, string>): Promise<Sent> {
  try {
    const answer = await fetch('/api/act', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Factory-Owner': '1' },
      body: JSON.stringify(body),
    });
    const said = await answer.json().catch(() => ({ ok: false, error: `The server answered ${answer.status}.` }));
    return said.ok ? { ok: true, said: said.said || '' } : { ok: false, error: said.error || `The server answered ${answer.status}.` };
  } catch {
    return { ok: false, error: 'The dashboard\'s server is not answering.' };
  }
}

/** Press one of the factory's buttons. */
export async function press(project: string, actionId: string, value: string, key?: string, label?: string): Promise<Sent> {
  const done = await send({ project, act: 'press', action_id: actionId, value });
  if (done.ok && key) live.sent[key] = { label: label || actionId, at: live.now };
  return done;
}

/** Reply in a thread, as the owner would in Slack. */
export async function reply(project: string, channel: string, thread: string, text: string, key?: string): Promise<Sent> {
  const done = await send({ project, act: 'say', channel: channel || '', thread, text });
  if (done.ok && key) live.sent[key] = { label: 'Replied', at: live.now };
  return done;
}

/** A new message, as at the top of the project's channel: to its
 * executive, or a new goal where it has none. */
export async function startConversation(project: string, text: string): Promise<Sent> {
  const done = await send({ project, act: 'new', text });
  if (done.ok) {
    const thread = /thread (\S+)/.exec(done.said)?.[1] ?? '';
    live.started[project] = [...(live.started[project] || []), { text, at: live.now, thread }];
  }
  return done;
}

export async function conversation(where: 'goal' | 'project', id: string, thread?: string): Promise<{ lines: Line[]; earlier: boolean } | null> {
  const query = new URLSearchParams({ where, id });
  if (thread) query.set('thread', thread);
  try {
    const answer = await fetch(`/api/thread?${query}`, { cache: 'no-store' });
    return answer.ok ? await answer.json() : null;
  } catch {
    return null;
  }
}

export const inboxOf = (project: string): Item[] =>
  (live.picture?.inbox || []).filter((item) => item.project === project);
