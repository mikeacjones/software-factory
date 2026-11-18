// A project as a graph: its executive, goals, sub-goals and beads, each a
// node; and a bead's pipeline as a graph of its flow's steps and routes.

import { Position, type Edge, type Node } from '@xyflow/svelte';
import { beadState, model, money, plural, short, span, toneOf } from './format';
import { layout } from './layout';
import { executiveAgents, foremanAgents, goalAgents } from './agents';
import type { Bead, Goal, Item, Project, Step, SubGoal } from './types';

export type Ref =
  | { level: 'executive' }
  | { level: 'goal'; goal: Goal }
  | { level: 'subgoal'; goal: Goal; sub: SubGoal }
  | { level: 'bead'; goal?: Goal; sub?: SubGoal; bead: Bead }
  | { level: 'group' };

/** The size of every kind of card, in the graph's own pixels. The cards
 * are drawn at exactly these sizes (their text is clipped to fit), so the
 * layout never places one over another whatever fonts the browser has. */
export const SIZE = {
  tier: { width: 270, height: 118 },
  bead: { width: 250, height: 90 },
  step: { width: 150, height: 52 },
  end: { width: 76, height: 24 },
};
/** The frame around a pipeline drawn on the map: its header, and the
 * space around its steps. */
const FRAME = { header: 50, pad: 14 };

/** Which beads have their whole pipeline drawn on the map. */
export type Pipelines = 'active' | 'all' | 'cards';

/** Sub-goals in these states are folded: their beads are not drawn. */
const FOLDED = ['landed', 'dropped'];

const running = (bead: Bead) => bead.status === 'running' && (!!bead.run || !!bead.command);

export function beadWhere(bead: Bead, now: number): string {
  if (bead.run) return `${bead.run.step} · ${bead.run.role} on ${model(bead.run.model)} · ${span(now - bead.run.since)}`;
  if (bead.command) return `${bead.command.step} · a check · ${span(now - bead.command.since)}`;
  if (bead.asked) return `${bead.asked.step} asks you a question`;
  if (!bead.unit) return 'not started';
  if (bead.status === 'running') return `${bead.step} · between steps`;
  if (bead.status === 'parked') return `parked at ${bead.step}`;
  if (bead.status === 'ended') return bead.pull_request ? `ended · PR #${String(bead.pull_request).split('/').pop()}` : 'ended';
  return bead.step || '';
}

export function nodeId(ref: Ref): string {
  switch (ref.level) {
    case 'executive': return 'exec';
    case 'group': return 'loose';
    case 'goal': return `goal:${ref.goal.id}`;
    case 'subgoal': return `sub:${ref.goal.id}/${ref.sub.id}`;
    case 'bead': return `bead:${ref.goal?.id ?? ''}/${ref.sub?.id ?? ''}/${ref.bead.unit || ref.bead.bead}${ref.sub?.ship === ref.bead ? '/ship' : ''}`;
  }
}

export interface Drawing {
  /** How beads and tiers are drawn unless the owner chose for one. */
  pipelines: Pipelines;
  /** The owner's choice for single cards: true open, false folded. */
  open: Record<string, boolean>;
  ontoggle: (id: string, open: boolean) => void;
}

type Inner = { nodes: (Node & { width: number; height: number })[]; edges: Edge[]; width: number; height: number };

export function projectGraph(project: Project, inbox: Item[], chosen: string | undefined, now: number, drawing: Drawing = { pipelines: 'active', open: {}, ontoggle: () => {} }) {
  const { pipelines } = drawing;
  const nodes: (Node & { width: number; height: number })[] = [];
  const children: Node[] = [];
  const edges: Edge[] = [];
  const refs = new Map<string, Ref>();
  const isOpen = (id: string, otherwise: boolean) => drawing.open[id] ?? otherwise;
  /** A card drawn as a frame holding a graph of its own. */
  const frame = (id: string, ref: Ref, data: Record<string, unknown>, inner: Inner) => {
    const width = Math.max(SIZE.bead.width + 120, inner.width + FRAME.pad * 2);
    const height = inner.height + FRAME.header + FRAME.pad;
    nodes.push({ id, type: 'pipeline', position: { x: 0, y: 0 }, width, height, data: { ...data, size: { width, height }, ontoggle: () => drawing.ontoggle(id, false) } });
    for (const node of inner.nodes) {
      refs.set(node.id, ref);
      children.push({ ...node, parentId: id, extent: 'parent', position: { x: node.position.x + FRAME.pad, y: node.position.y + FRAME.header } });
    }
    for (const edge of inner.edges) edges.push({ ...edge, zIndex: 1 });
  };
  /** A tier: a card, or, opened, a frame of its agents at work. */
  const tier = (ref: Ref, data: Record<string, unknown>, agents?: (prefix: string) => Inner, running = false) => {
    const id = nodeId(ref);
    refs.set(id, ref);
    const open = !!agents && isOpen(id, pipelines === 'all' || (pipelines === 'active' && running));
    if (open && agents) {
      frame(id, ref, {
        id: data.label,
        title: data.title,
        tone: data.tone,
        word: data.word,
        where: (data.facts as string[]).join(' · '),
        going: data.busy,
        asks: (data.waiting as number) > 0,
        chosen: chosen === id,
      }, agents(`${id}::`));
      return id;
    }
    nodes.push({
      id,
      type: 'tier',
      data: { ...data, level: ref.level, chosen: chosen === id, size: SIZE.tier, ontoggle: agents ? () => drawing.ontoggle(id, true) : undefined },
      position: { x: 0, y: 0 },
      ...SIZE.tier,
    });
    return id;
  };
  const link = (source: string, target: string, kind: 'hot' | 'waiting' | '' = '') => {
    edges.push({
      id: `${source}->${target}`,
      source,
      target,
      type: 'smoothstep',
      animated: kind === 'hot',
      class: kind,
    });
  };
  const drawn = (bead: Bead, id: string) => {
    if (!bead.steps?.length) return false;
    const active = bead.status === 'running' || bead.status === 'parked' || !!bead.asked;
    return isOpen(id, pipelines === 'all' || (pipelines === 'active' && active));
  };
  const beadNode = (bead: Bead, goal?: Goal, sub?: SubGoal, ship = false) => {
    const ref: Ref = { level: 'bead', goal, sub, bead };
    const id = nodeId(ref);
    refs.set(id, ref);
    const [tone, word] = beadState(bead);
    const data = {
      id: short(bead.bead, project.id),
      title: bead.title || short(bead.bead, project.id),
      tone,
      word,
      where: beadWhere(bead, now),
      steps: bead.steps || [],
      here: bead.run?.step || bead.command?.step || bead.step,
      going: bead.status === 'running',
      over: bead.status === 'ended',
      ship,
      asks: !!bead.asked,
      chosen: chosen === id,
      cost: bead.cost ? money(bead.cost) : '',
      flow: bead.flow,
    };
    if (!drawn(bead, id)) {
      const ontoggle = bead.steps?.length ? () => drawing.ontoggle(id, true) : undefined;
      nodes.push({ id, type: 'bead', position: { x: 0, y: 0 }, ...SIZE.bead, data: { ...data, size: SIZE.bead, ontoggle } });
      return id;
    }
    // The bead as a frame holding its whole pipeline.
    frame(id, ref, { ...data, where: `${data.where}${data.flow ? ` · flow ${data.flow}` : ''}` }, pipelineGraph(bead, 'TB', `${id}::`, true));
    return id;
  };

  const waitingFor = (goal: string) => inbox.filter((item) => item.goal === goal).length;
  const exec = project.executive;
  let root: string | null = null;
  if (exec) {
    const facts = [plural(exec.wakes, 'wake'), money(exec.cost)];
    if (exec.conversations?.length) facts.unshift(plural(exec.conversations.length, 'conversation'));
    root = tier({ level: 'executive' }, {
      label: 'Executive',
      title: exec.run ? `${exec.run.role} at work` : 'Asleep until you write',
      tone: exec.run ? 'running' : 'todo',
      word: exec.run ? 'Working' : 'Asleep',
      facts,
      busy: !!exec.run,
      waiting: inbox.filter((item) => !item.goal && item.kind === 'proposal').length,
    }, exec.agents ? (prefix) => executiveAgents(exec.agents!, exec.run?.role, prefix) : undefined, !!exec.run);
  }

  for (const goal of project.goals || []) {
    const [tone, word] = toneOf(goal.phase);
    const subs = goal.subgoals || [];
    const own = goal.beads || [];
    const beads = [...subs.flatMap((sub) => sub.beads || []), ...own];
    const done = beads.filter((bead) => ['absorbed', 'landed'].includes(bead.state || '')).length;
    const busy = !!goal.run || beads.some(running) || subs.some((sub) => sub.ship && running(sub.ship));
    const waiting = waitingFor(goal.id);
    const facts = [money(goal.cost)];
    if (subs.length) facts.unshift(plural(subs.length, 'sub-goal'));
    if (goal.run) facts.unshift(`${goal.run.role} at work`);
    const gid = tier({ level: 'goal', goal }, {
      label: `Goal · ${short(goal.id, project.id)}`,
      title: goal.objective || '(no charter yet)',
      tone,
      word,
      facts,
      busy,
      waiting,
      progress: [done, beads.length],
    }, goal.agents ? (prefix) => goalAgents(goal.agents!, goal.run?.role, prefix) : undefined, !!goal.run && goal.run.role !== 'foreman');
    if (root) link(root, gid, waiting ? 'waiting' : busy ? 'hot' : '');

    for (const sub of subs) {
      const [stone, sword] = toneOf(sub.state);
      const sbeads = sub.beads || [];
      const sdone = sbeads.filter((bead) => ['absorbed', 'landed'].includes(bead.state || '')).length;
      const sbusy = sbeads.some(running) || (!!sub.ship && running(sub.ship));
      const folded = FOLDED.includes(sub.state || '');
      const facts: string[] = [];
      if (sub.foreman?.wakes) facts.push(`foreman: ${plural(sub.foreman.wakes, 'wake')}`);
      if (sub.foreman?.waiting) facts.push(`reports ${sub.foreman.waiting}`);
      if (sub.pull_request) facts.push(`PR #${String(sub.pull_request).split('/').pop()}`);
      if (folded) facts.push(`${sbeads.length} beads folded`);
      const sid = tier({ level: 'subgoal', goal, sub }, {
        label: 'Sub-goal',
        title: sub.title || sub.id,
        tone: stone,
        word: sword,
        facts,
        busy: sbusy,
        waiting: sbeads.filter((bead) => bead.asked).length,
        progress: [sdone, sbeads.length],
      }, goal.agents && sub.foreman?.wakes ? (prefix) => foremanAgents(goal.agents!, sub.id, sub.foreman?.wakes ?? 0, goal.run?.role === 'foreman' && !folded, prefix) : undefined);
      link(gid, sid, sbusy ? 'hot' : '');
      if (folded) continue;
      for (const bead of sbeads) link(sid, beadNode(bead, goal, sub), running(bead) ? 'hot' : bead.asked ? 'waiting' : '');
      if (sub.ship) link(sid, beadNode(sub.ship, goal, sub, true), running(sub.ship) ? 'hot' : '');
    }
    for (const bead of own) link(gid, beadNode(bead, goal), running(bead) ? 'hot' : bead.asked ? 'waiting' : '');
  }

  if (project.loose?.length) {
    const lid = tier({ level: 'group' }, {
      label: 'By hand',
      title: 'Pipelines run in no goal',
      tone: 'running',
      word: plural(project.loose.length, 'unit'),
      facts: [],
      busy: project.loose.some(running),
      waiting: 0,
    });
    if (root) link(root, lid);
    for (const bead of project.loose) link(lid, beadNode(bead), running(bead) ? 'hot' : '');
  }

  // Only the cards and frames are laid out; a pipeline's steps keep their
  // places inside its frame, and come after it, as Svelte Flow needs.
  const placed = layout(nodes, edges.filter((edge) => !edge.id.includes('::')), { rankdir: 'TB', ranksep: 70, nodesep: 24 });
  return { nodes: [...placed, ...children], edges, refs };
}

/** A bead's pipeline: every step of its flow and where each outcome leads.
 * A route the bead has taken is drawn heavier, with how often; a route
 * back to an earlier step, a cycle, is dashed. `prefix` keeps the ids of
 * one pipeline's steps apart from another's on the same map. */
export function pipelineGraph(bead: Bead, rankdir: 'LR' | 'TB' = 'TB', prefix = '', quiet = false) {
  const steps: Step[] = bead.steps || [];
  const order = new Map(steps.map((step, index) => [step.id, index]));
  const here = bead.run?.step || bead.command?.step || bead.step;
  const taken = bead.taken || {};
  const nodes: (Node & { width: number; height: number })[] = [];
  const edges: Edge[] = [];
  const ends = new Map<string, string>();
  for (const step of steps) {
    let stateOf: string = step.rounds ? 'done' : 'todo';
    if (step.id === here && bead.status !== 'ended') {
      stateOf = bead.asked ? 'asks' : bead.status === 'running' ? 'now' : 'stopped';
    }
    nodes.push({
      id: prefix + step.id,
      type: 'step',
      position: { x: 0, y: 0 },
      ...SIZE.step,
      data: { ...step, state: stateOf, end: false, spent: bead.spent?.[step.id] ?? 0, size: SIZE.step },
    });
    for (const route of step.routes || []) {
      // A route out of the flow (park, ask, absorb…) ends at a mark of its
      // own beside the step, so that no one mark pulls every step to it.
      const inside = order.has(route.to);
      const target = prefix + (inside ? route.to : `${step.id}→${route.to}`);
      if (!inside) ends.set(target, route.to);
      const back = inside && (order.get(route.to) ?? 0) <= (order.get(step.id) ?? 0);
      const times = taken[`${step.id}:${route.outcome}`] || 0;
      const hot = times > 0 && route.to === here && bead.status === 'running';
      const label = `${route.outcome}${route.max ? ` ≤${route.max}` : ''}${times ? ` · ×${times}` : ''}`;
      edges.push({
        id: `${prefix}${step.id}-${route.outcome}-${route.to}`,
        source: prefix + step.id,
        target,
        // On the map a route not taken is drawn without words.
        label: quiet && !times ? undefined : label,
        type: back ? 'default' : 'smoothstep',
        animated: hot,
        class: [back ? 'back' : '', hot ? 'hot' : times ? 'walked' : 'unwalked'].join(' '),
        // The next step in the flow's own order is its main path.
        data: { weight: inside && (order.get(route.to) ?? 0) === (order.get(step.id) ?? 0) + 1 ? 20 : inside ? 2 : 1 },
      });
    }
  }
  for (const [id, name] of ends) {
    nodes.push({ id, type: 'step', position: { x: 0, y: 0 }, ...SIZE.end, data: { id: name, kind: 'end', rounds: 0, state: 'end', end: true, size: SIZE.end } });
  }
  const { placed, row, col } = arrange(steps, nodes, rankdir, prefix);
  if (rankdir === 'TB') {
    for (const edge of edges) {
      const from = edge.source.slice(prefix.length);
      const to = edge.target.slice(prefix.length);
      Object.assign(edge, sides(from, to, row, col, to.includes('→')), { type: 'smoothstep', pathOptions: { offset: 18, borderRadius: 10 } });
    }
  }
  // Where the steps came to lie, with the top-left corner at nought.
  const left = Math.min(...placed.map((node) => node.position.x));
  const top = Math.min(...placed.map((node) => node.position.y));
  const moved = placed.map((node) => ({ ...node, position: { x: node.position.x - left, y: node.position.y - top } }));
  const width = Math.max(0, ...moved.map((node) => node.position.x + (node.width ?? 0)));
  const height = Math.max(0, ...moved.map((node) => node.position.y + (node.height ?? 0)));
  return { nodes: moved, edges, width, height };
}

/** Where a pipeline's steps go. The main path (from the first step, each
 * step's route to the soonest step after it in the flow) runs straight
 * down one column, in the flow's order. A step off it, such as `fix`,
 * stands in a column beside it, level with the last step on the path
 * that leads to it. A route out of the flow is a mark to the left of the
 * step it leaves. Laid out so, every pipeline of one flow looks the same,
 * and a cycle reads as a line back up the column. */
function arrange(steps: Step[], nodes: (Node & { width: number; height: number })[], rankdir: 'LR' | 'TB', prefix: string) {
  const order = new Map(steps.map((step, index) => [step.id, index]));
  // The longest chain of forward routes is the way a bead goes when
  // nothing sends it back: that is the main path.
  const forwardOf = (step: Step) =>
    (step.routes || []).map((route) => route.to).filter((to) => (order.get(to) ?? -1) > (order.get(step.id) ?? 0));
  const longest = new Map<string, number>();
  for (const step of [...steps].reverse()) {
    longest.set(step.id, 1 + Math.max(0, ...forwardOf(step).map((to) => longest.get(to) ?? 0)));
  }
  const spine: string[] = [];
  let at: Step | undefined = steps[0];
  while (at && !spine.includes(at.id)) {
    spine.push(at.id);
    const next: string | undefined = forwardOf(at).sort((a, b) =>
      (longest.get(b) ?? 0) - (longest.get(a) ?? 0) || (order.get(a) ?? 0) - (order.get(b) ?? 0))[0];
    at = next ? steps[order.get(next)!] : undefined;
  }
  const row = new Map(spine.map((id, index) => [id, index]));
  const col = new Map(spine.map((id) => [id, 0]));
  // Steps off the main path, beside the last step on it that leads there.
  const taken = new Set<string>();
  for (const step of steps) {
    if (row.has(step.id)) continue;
    const from = steps
      .filter((one) => row.has(one.id) && (one.routes || []).some((route) => route.to === step.id))
      .map((one) => row.get(one.id)!);
    from.sort((a, b) => a - b);
    let r = from.length ? from[Math.floor((from.length - 1) / 2)] : spine.length;
    let c = 1;
    while (taken.has(`${r}/${c}`)) c++;
    taken.add(`${r}/${c}`);
    row.set(step.id, r);
    col.set(step.id, c);
  }
  const W = SIZE.step.width + 90;
  const H = SIZE.step.height + 54;
  const ends = new Map<string, number>();
  const placed = nodes.map((node) => {
    const local = node.id.slice(prefix.length);
    let x: number;
    let y: number;
    if (local.includes('→')) {
      // A mark beside the step it leaves, to its left.
      const from = local.slice(0, local.indexOf('→'));
      const n = ends.get(from) ?? 0;
      ends.set(from, n + 1);
      x = (col.get(from) ?? 0) * W - 24 - (n + 1) * (SIZE.end.width + 10);
      // In the gap below its step, so the route drops down to it.
      y = (row.get(from) ?? 0) * H + SIZE.step.height + 16;
      if ((col.get(from) ?? 0) > 0) x = (col.get(from) ?? 0) * W + SIZE.step.width + 24 + n * (SIZE.end.width + 10);
    } else {
      x = (col.get(local) ?? 0) * W;
      y = (row.get(local) ?? 0) * H;
    }
    const across = rankdir === 'LR';
    return {
      ...node,
      targetPosition: across ? Position.Left : Position.Top,
      sourcePosition: across ? Position.Right : Position.Bottom,
      position: across ? { x: y * 1.6, y: x * 0.7 } : { x, y },
    };
  });
  return { placed, row, col };
}

/** Which sides a route of a pipeline laid out top-down leaves and enters
 * by: down the main path, out to the right into a side step and back in
 * from its left, and up the right of the column for a cycle. */
function sides(from: string, to: string, row: Map<string, number>, col: Map<string, number>, mark: boolean) {
  if (mark) return {};
  const [fr, fc, tr, tc] = [row.get(from) ?? 0, col.get(from) ?? 0, row.get(to) ?? 0, col.get(to) ?? 0];
  if (fc === tc && tr === fr + 1) return {};
  if (fc < tc) return { sourceHandle: 'right', targetHandle: 'left-in' };
  if (fc > tc) return { sourceHandle: 'left', targetHandle: 'right-in' };
  if (tr <= fr) return { sourceHandle: 'right', targetHandle: 'right-in' };
  return { sourceHandle: 'left', targetHandle: 'left-in' };
}

/** What a node of a project's map stands for, in the picture as it is now. */
export function findRef(project: Project, id: string | undefined): Ref | null {
  if (!id) return null;
  id = id.split('::')[0];
  if (id === 'exec') return project.executive ? { level: 'executive' } : null;
  if (id === 'loose') return { level: 'group' };
  const [level, rest] = [id.slice(0, id.indexOf(':')), id.slice(id.indexOf(':') + 1)];
  const parts = rest.split('/');
  const goal = (project.goals || []).find((one) => one.id === parts[0]);
  if (level === 'goal') return goal ? { level: 'goal', goal } : null;
  const sub = goal?.subgoals?.find((one) => one.id === parts[1]);
  if (level === 'sub') return goal && sub ? { level: 'subgoal', goal, sub } : null;
  if (level === 'bead') {
    const key = parts[2];
    if (!goal) {
      const bead = (project.loose || []).find((one) => (one.unit || one.bead) === key);
      return bead ? { level: 'bead', bead } : null;
    }
    if (!parts[1]) {
      const bead = (goal.beads || []).find((one) => (one.unit || one.bead) === key);
      return bead ? { level: 'bead', goal, bead } : null;
    }
    if (parts[3] === 'ship' && sub?.ship) return { level: 'bead', goal, sub, bead: sub.ship };
    const bead = sub?.beads?.find((one) => (one.unit || one.bead) === key);
    return bead && sub ? { level: 'bead', goal, sub, bead } : null;
  }
  return null;
}
