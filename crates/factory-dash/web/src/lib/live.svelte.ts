// The live state of the page: the picture, read every two seconds while the
// page is looked at, the server's clock, where the reader is, and what the
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

let tag: string | null = null;

async function poll() {
  try {
    const answer = await fetch('/api/snapshot', {
      cache: 'no-store',
      headers: tag ? { 'If-None-Match': tag } : {},
    });
    const serverNow = Number(answer.headers.get('x-now'));
    if (serverNow) live.skew = serverNow - Date.now() / 1000;
    if (answer.status === 200) {
      const picture: Picture = await answer.json();
      tag = answer.headers.get('etag');
      live.picture = picture;
      // What was sent and has left the inbox has been taken in.
      const waiting = new Set((picture.inbox || []).map((item) => item.key));
      for (const key of Object.keys(live.sent)) if (!waiting.has(key)) delete live.sent[key];
    }
    if (answer.status === 200 || answer.status === 304) live.heard = Date.now() / 1000;
  } catch {
    // The pulse says so if it goes on.
  }
  setTimeout(poll, document.hidden ? 15000 : 2000);
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
  poll();
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
