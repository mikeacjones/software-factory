// A project as a graph: its executive, goals, sub-goals and beads, each a
// node; and a bead's pipeline as a graph of its flow's steps and routes.

import type { Edge, Node } from '@xyflow/svelte';
import { beadState, model, money, plural, short, span, toneOf } from './format';
import { layout } from './layout';
import type { Bead, Goal, Item, Project, Step, SubGoal } from './types';

export type Ref =
  | { level: 'executive' }
  | { level: 'goal'; goal: Goal }
  | { level: 'subgoal'; goal: Goal; sub: SubGoal }
  | { level: 'bead'; goal?: Goal; sub?: SubGoal; bead: Bead }
  | { level: 'group' };

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

export function projectGraph(project: Project, inbox: Item[], chosen: string | undefined, now: number) {
  const nodes: (Node & { width: number; height: number })[] = [];
  const edges: Edge[] = [];
  const refs = new Map<string, Ref>();
  const tier = (ref: Ref, data: Record<string, unknown>, height = 92) => {
    const id = nodeId(ref);
    refs.set(id, ref);
    nodes.push({ id, type: 'tier', data: { ...data, level: ref.level, chosen: chosen === id }, position: { x: 0, y: 0 }, width: 270, height });
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
  const beadNode = (bead: Bead, goal?: Goal, sub?: SubGoal, ship = false) => {
    const ref: Ref = { level: 'bead', goal, sub, bead };
    const id = nodeId(ref);
    refs.set(id, ref);
    const [tone, word] = beadState(bead);
    nodes.push({
      id,
      type: 'bead',
      position: { x: 0, y: 0 },
      width: 250,
      height: 78,
      data: {
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
      },
    });
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
    }, 84);
  }

  for (const goal of project.goals || []) {
    const [tone, word] = toneOf(goal.phase);
    const subs = goal.subgoals || [];
    const beads = subs.flatMap((sub) => sub.beads || []);
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
    }, 104);
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
      }, 96);
      link(gid, sid, sbusy ? 'hot' : '');
      if (folded) continue;
      for (const bead of sbeads) link(sid, beadNode(bead, goal, sub), running(bead) ? 'hot' : bead.asked ? 'waiting' : '');
      if (sub.ship) link(sid, beadNode(sub.ship, goal, sub, true), running(sub.ship) ? 'hot' : '');
    }
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
    }, 70);
    if (root) link(root, lid);
    for (const bead of project.loose) link(lid, beadNode(bead), running(bead) ? 'hot' : '');
  }

  return { nodes: layout(nodes, edges, { ranksep: 60, nodesep: 12 }), edges, refs };
}

/** A bead's pipeline: every step of its flow, and where each outcome
 * leads, with the way it has come drawn heavier. */
export function pipelineGraph(bead: Bead, rankdir: 'LR' | 'TB' = 'LR') {
  const steps: Step[] = bead.steps || [];
  const order = new Map(steps.map((step, index) => [step.id, index]));
  const here = bead.run?.step || bead.command?.step || bead.step;
  const nodes: (Node & { width: number; height: number })[] = [];
  const edges: Edge[] = [];
  const ends = new Set<string>();
  for (const step of steps) {
    let stateOf: string = step.rounds ? 'done' : 'todo';
    if (step.id === here && bead.status !== 'ended') {
      stateOf = bead.asked ? 'asks' : bead.status === 'running' ? 'now' : 'stopped';
    }
    nodes.push({
      id: step.id,
      type: 'step',
      position: { x: 0, y: 0 },
      width: 140,
      height: 46,
      data: { ...step, state: stateOf, end: false },
    });
    for (const route of step.routes || []) {
      // A route out of the flow (park, ask, absorb…) ends at a mark of its
      // own beside the step, so that no one mark pulls every step to it.
      const target = order.has(route.to) ? route.to : `${step.id}→${route.to}`;
      if (!order.has(route.to)) ends.add(target);
      const back = order.has(route.to) && (order.get(route.to) ?? 0) <= (order.get(step.id) ?? 0);
      const walked = step.rounds > 0 && (order.has(route.to) ? (steps[order.get(route.to)!].rounds > 0) : false);
      const hot = route.to === here && bead.status === 'running' && step.rounds > 0;
      edges.push({
        id: `${step.id}-${route.outcome}-${route.to}`,
        source: step.id,
        target,
        label: route.max ? `${route.outcome} ≤${route.max}` : route.outcome,
        type: back ? 'default' : 'smoothstep',
        animated: hot,
        class: [back ? 'back' : '', hot ? 'hot' : walked ? 'walked' : ''].join(' '),
      });
    }
  }
  for (const end of ends) {
    const name = end.slice(end.indexOf('→') + 1);
    nodes.push({ id: end, type: 'step', position: { x: 0, y: 0 }, width: 70, height: 22, data: { id: name, kind: 'end', rounds: 0, state: 'end', end: true } });
  }
  // Back edges would pull the layout into knots: lay out with the forward
  // ones and draw them all.
  const forward = edges.filter((edge) => !String(edge.class).includes('back'));
  return { nodes: layout(nodes, forward, { rankdir, ranksep: rankdir === 'TB' ? 34 : 46, nodesep: 18 }), edges };
}

/** What a node of a project's map stands for, in the picture as it is now. */
export function findRef(project: Project, id: string | undefined): Ref | null {
  if (!id) return null;
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
    if (parts[3] === 'ship' && sub?.ship) return { level: 'bead', goal, sub, bead: sub.ship };
    const bead = sub?.beads?.find((one) => (one.unit || one.bead) === key);
    return bead && sub ? { level: 'bead', goal, sub, bead } : null;
  }
  return null;
}
