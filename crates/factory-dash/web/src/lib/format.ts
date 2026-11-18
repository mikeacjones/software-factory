// Words for times, money and states. Times are kept as times in the
// picture and turned into "4 min" here, against the server's clock.

export function span(secs: number): string {
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs} s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min`;
  const hours = Math.floor(mins / 60);
  if (hours < 48) return `${hours} h${mins % 60 ? ` ${mins % 60} min` : ''}`;
  return `${Math.floor(hours / 24)} d${hours % 24 ? ` ${hours % 24} h` : ''}`;
}

export function clock(at: number, now: number): string {
  const day = new Date(at * 1000);
  const today = new Date(now * 1000).toDateString() === day.toDateString();
  const time = day.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  return today ? time : `${day.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' })} ${time}`;
}

export const ago = (at: number | null | undefined, now: number) => (at ? `${span(now - at)} ago` : '');

export const pct = (share: number) => `${Math.round((share || 0) * 100)}%`;

export const plural = (n: number | undefined, thing: string) =>
  `${Number(n) || 0} ${thing}${Number(n) === 1 ? '' : 's'}`;

export function money(usd: number | undefined): string {
  const value = Math.max(0, usd || 0);
  return `$${value >= 100 ? Math.round(value).toLocaleString() : value.toFixed(2)}`;
}

export const model = (name?: string) => String(name || '').replace(/^claude-/, '');

/** A bead's ID without its project's prefix. */
export function short(bead: string | undefined, project: string | undefined): string {
  if (bead && project && bead.startsWith(`${project}-`)) return bead.slice(project.length + 1);
  return bead || '';
}

export type Tone = 'running' | 'done' | 'todo' | 'wait' | 'bad' | '';

const tones: Record<string, Tone> = {
  running: 'running', shipping: 'running', working: 'running', open: 'running',
  absorbed: 'done', landed: 'done', complete: 'done', ended: 'done', merged: 'done',
  planned: 'todo', queued: 'todo', idle: 'todo',
  parked: 'wait', waiting: 'wait', proposed: 'wait', proposed_complete: 'wait', held: 'wait', intake: 'wait',
  failed: 'bad', refused: 'bad', dropped: 'bad', abandoned: 'bad', stopped: 'bad',
};

/** The tone a state wears, and the word beside it. */
export function toneOf(word?: string | null): [Tone, string] {
  const key = String(word || '').toLowerCase();
  const text = String(word || 'unknown').replace(/_/g, ' ');
  return [tones[key] || '', text.charAt(0).toUpperCase() + text.slice(1)];
}

/** A bead's state as the page shows it: its pipeline's status wins over
 * the goal's word for it while the pipeline is stopped. */
export function beadState(bead: { state?: string; status?: string }): [Tone, string] {
  if (bead.status === 'parked') return ['wait', 'Parked'];
  if (bead.status && !['running', 'ended'].includes(bead.status) && bead.state === 'running') return toneOf(bead.status);
  return toneOf(bead.state);
}

export function slackLink(base: string | null | undefined, thread: { channel: string; ts: string } | null | undefined): string | null {
  if (!base || !thread?.channel || !thread.ts) return null;
  return `${base}/archives/${thread.channel}/p${thread.ts.replace('.', '')}`;
}

export const kindWords: Record<string, string> = {
  charter: 'Charter to start',
  intake: 'Shaping a goal',
  completion: 'Done? Accept',
  request: 'Bead changes',
  question: 'Question',
  proposal: 'Proposal',
  step_question: 'Pipeline question',
  parked: 'Parked by hand',
};
