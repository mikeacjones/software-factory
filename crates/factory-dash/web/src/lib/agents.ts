// A tier drawn as its agents at work: what woke them, each agent with its
// runs, and what they decided, every one counted from the tier's log.

import type { Edge, Node } from '@xyflow/svelte';
import { model as modelName, money } from './format';
import type { Agents } from './types';

export const CHIP = { width: 176, height: 36 };
export const AGENT = { width: 196, height: 68 };
const GAP = { x: 18, row: 64 };

type Sized = Node & { width: number; height: number };

/** A reason something was woken, with the names and numbers taken out,
 * so that reasons of one shape add up together. */
export function shape(why: string): string {
  return why.replace(/`[^`]*`/g, '…').replace(/\d+/g, 'N').replace(/;.*$/, '').trim();
}

function grouped(counts: Record<string, number>, most = 5): [string, number][] {
  const sums = new Map<string, number>();
  for (const [why, n] of Object.entries(counts)) sums.set(shape(why), (sums.get(shape(why)) ?? 0) + n);
  const sorted = [...sums].sort((a, b) => b[1] - a[1]);
  if (sorted.length <= most) return sorted;
  const rest = sorted.slice(most - 1).reduce((sum, [, n]) => sum + n, 0);
  return [...sorted.slice(0, most - 1), ['anything else', rest]];
}

interface Agent {
  role: string;
  label: string;
  runs: number;
  failed: number;
  cost: number;
  model: string;
  now: boolean;
}

interface Outcome {
  key: string;
  label: string;
  count: number;
  from: string;
  tone?: 'warn' | 'bad' | 'good';
}

/** The graph of one tier: what woke it across the top, then each agent
 * as a column, with what it did listed under it on a bracket, so that who
 * decided what is plain. Positions start at nought. */
function rows(prefix: string, causes: [string, number][], agents: Agent[], outcomes: Outcome[], into: string) {
  const nodes: Sized[] = [];
  const edges: Edge[] = [];
  const INDENT = 26;
  const STACK = CHIP.height + 10;
  const COLUMN = Math.max(AGENT.width, CHIP.width + INDENT);
  const BETWEEN = 70;
  const shown = outcomes.filter((o) => o.count > 0);
  const columns = agents.map((agent) => ({ agent, did: shown.filter((o) => o.from === agent.role) }));
  const agentsWidth = columns.length * COLUMN + (columns.length - 1) * BETWEEN;
  const causesWidth = causes.length * CHIP.width + Math.max(0, causes.length - 1) * GAP.x;
  const width = Math.max(agentsWidth, causesWidth);
  const top = causes.length ? CHIP.height + GAP.row : 0;
  let x = (width - causesWidth) / 2;
  causes.forEach(([label, count], i) => {
    const id = `${prefix}cause${i}`;
    nodes.push({ id, type: 'chip', position: { x, y: 0 }, ...CHIP, data: { label, count, kind: 'cause', size: CHIP } });
    edges.push({ id: `${id}->`, source: id, target: `${prefix}${into}`, type: 'smoothstep', label: `×${count}`, class: 'walked', zIndex: 1 });
    x += CHIP.width + GAP.x;
  });
  let height = top + AGENT.height;
  x = (width - agentsWidth) / 2;
  columns.forEach(({ agent, did }, index) => {
    const id = `${prefix}${agent.role}`;
    nodes.push({ id, type: 'agent', position: { x, y: top }, ...AGENT, data: { ...agent, model: modelName(agent.model), cost: agent.cost < 0 ? '' : money(agent.cost), size: AGENT } });
    did.forEach((o, i) => {
      const chip = `${prefix}out:${o.key}`;
      const y = top + AGENT.height + 22 + i * STACK;
      nodes.push({ id: chip, type: 'chip', position: { x: x + INDENT, y }, ...CHIP, data: { label: o.label, count: o.count, kind: 'outcome', tone: o.tone, size: CHIP } });
      edges.push({ id: `${id}->${chip}`, source: id, sourceHandle: 'under', target: chip, targetHandle: 'left-in', type: 'smoothstep', class: 'walked', zIndex: 1, pathOptions: { offset: 8, borderRadius: 6 } } as Edge);
      height = Math.max(height, y + CHIP.height);
    });
    // An agent another hands work to: the director asks the designer.
    const next = columns[index + 1]?.agent;
    if (agent.role === 'director' && next?.role === 'designer') {
      edges.push({ id: `${id}->designer`, source: id, sourceHandle: 'right', target: `${prefix}designer`, targetHandle: 'left-in', type: 'straight', label: 'asks', class: 'walked', zIndex: 1 });
    }
    x += COLUMN + BETWEEN;
  });
  return { nodes, edges, width, height };
}

const agentOf = (tally: Agents, role: string, label: string, running?: string): Agent | null => {
  const r = tally.roles[role];
  if (!r && running !== role) return null;
  return { role, label, runs: r?.runs ?? 0, failed: r?.failed ?? 0, cost: r?.cost ?? 0, model: r?.model ?? '', now: running === role };
};

/** A goal's director and designer. */
export function goalAgents(tally: Agents, running: string | undefined, prefix: string) {
  const k = tally.kinds;
  const agents = [
    agentOf(tally, 'director-intake', 'Director · charter', running),
    agentOf(tally, 'director', 'Director', running),
    agentOf(tally, 'designer', 'Designer', running),
  ].filter((a): a is Agent => !!a);
  const lead = agents.some((a) => a.role === 'director') ? 'director' : agents[0]?.role ?? 'director';
  if (!agents.length) agents.push({ role: 'director', label: 'Director', runs: 0, failed: 0, cost: 0, model: '', now: false });
  const causes = grouped(tally.wakes);
  if (k['owner.message'] && !causes.some(([why]) => why.includes('owner'))) causes.unshift(['you wrote', k['owner.message']]);
  const designer = agents.some((a) => a.role === 'designer') ? 'designer' : lead;
  const outcomes: Outcome[] = [
    { key: 'subgoal.opened', label: 'opened a sub-goal', count: k['subgoal.opened'] ?? 0, from: lead },
    { key: 'pipeline.started', label: 'started a pipeline', count: k['pipeline.started'] ?? 0, from: lead },
    { key: 'bead.filed', label: 'filed a bead', count: k['bead.filed'] ?? 0, from: lead },
    { key: 'bead.changed', label: 'changed a bead', count: k['bead.changed'] ?? 0, from: lead },
    { key: 'foreman.told', label: 'told a foreman', count: k['foreman.told'] ?? 0, from: lead },
    { key: 'executive.told', label: 'told the executive', count: k['executive.told'] ?? 0, from: lead },
    { key: 'subgoal.shipping', label: 'shipped a sub-goal', count: k['subgoal.shipping'] ?? 0, from: lead, tone: 'good' },
    { key: 'slack.need', label: 'asked you', count: (k['slack.need'] ?? 0) + (k['slack.prompt'] ?? 0), from: lead, tone: 'warn' },
    { key: 'director.refused', label: 'refused by code', count: k['director.refused'] ?? 0, from: lead, tone: 'bad' },
    { key: 'design.proposed', label: 'proposed a design', count: k['design.proposed'] ?? 0, from: designer },
    { key: 'design.accepted', label: 'design accepted', count: k['design.accepted'] ?? 0, from: lead, tone: 'good' },
    { key: 'design.sent_back', label: 'design sent back', count: k['design.sent_back'] ?? 0, from: lead, tone: 'warn' },
  ];
  return rows(prefix, causes, agents, outcomes, lead);
}

/** A project's executive. */
export function executiveAgents(tally: Agents, running: string | undefined, prefix: string) {
  const k = tally.kinds;
  const agent = agentOf(tally, 'executive', 'Executive', running) ?? { role: 'executive', label: 'Executive', runs: 0, failed: 0, cost: 0, model: '', now: false };
  const causes: [string, number][] = k['owner.message'] ? [['you wrote', k['owner.message']]] : [];
  if (k['executive.told']) causes.push(['a director told it', k['executive.told']]);
  const outcomes: Outcome[] = [
    { key: 'goal.opened', label: 'opened a goal', count: k['goal.opened'] ?? 0, from: 'executive', tone: 'good' },
    { key: 'executive.decided', label: 'decided', count: k['executive.decided'] ?? 0, from: 'executive' },
    { key: 'bead.closed', label: 'closed a bead', count: k['bead.closed'] ?? 0, from: 'executive' },
    { key: 'slack.need', label: 'asked you', count: k['slack.need'] ?? 0, from: 'executive', tone: 'warn' },
    { key: 'executive.refused', label: 'refused by code', count: k['executive.refused'] ?? 0, from: 'executive', tone: 'bad' },
  ];
  return rows(prefix, causes, [agent], outcomes, 'executive');
}

/** A sub-goal's foreman, from what its goal's log says of the sub-goal. */
export function foremanAgents(tally: Agents, sub: string, wakes: number, running: boolean, prefix: string) {
  const s = tally.subgoals[sub] ?? {};
  const reports = Object.entries(s).filter(([key]) => key.startsWith('foreman.decided:'));
  // The log does not say which sub-goal a foreman's run was for, so what
  // one sub-goal's foreman cost is not known: none is shown.
  const agent: Agent = { role: 'foreman', label: 'Foreman', runs: wakes, failed: 0, cost: -1, model: '', now: running };
  const causes: [string, number][] = [];
  if (s['foreman.told']) causes.push(['the director wrote', s['foreman.told']]);
  if (s['design.accepted']) causes.push(['a design was accepted', s['design.accepted']]);
  const outcomes: Outcome[] = reports.map(([key, count]) => {
    const report = key.slice('foreman.decided:'.length);
    return { key, label: `reported ${report}`, count, from: 'foreman', tone: report === 'ready' ? 'good' : report === 'escalate' ? 'warn' : undefined };
  });
  if (s['design.proposed']) outcomes.push({ key: 'design.proposed', label: 'its design proposed', count: s['design.proposed'], from: 'foreman' });
  if (s['design.sent_back']) outcomes.push({ key: 'design.sent_back', label: 'its design sent back', count: s['design.sent_back'], from: 'foreman', tone: 'warn' });
  return rows(prefix, causes, [agent], outcomes, 'foreman');
}
