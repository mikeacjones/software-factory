// The picture of the factory as the server sends it. Every field may be
// missing: a record that changed its shape leaves a gap, not an error.

export interface Window {
  utilization: number;
  resets_at: number | null;
  length: number;
  status?: string;
  read_at?: number;
  per_hour?: number | null;
  full_at?: number | null;
  trail?: [number, number][];
}

export interface Run {
  where: 'unit' | 'goal' | 'project';
  id: string;
  file: string;
  role: string;
  model?: string;
  effort?: string;
  account?: string | null;
  step?: string;
  since: number;
  active_at?: number;
  project?: string;
  bead?: string;
  title?: string;
  goal?: string;
  what?: string;
}

export interface Account {
  name: string;
  five_hour: Window;
  seven_day: Window;
  models?: Record<string, Window>;
  blocked?: Record<string, number>;
  concern?: string | null;
  spent_hour: number;
  spent_day: number;
  handed_at?: number | null;
  runs: Run[];
}

export interface Route {
  outcome: string;
  to: string;
  max?: number | null;
}

export interface Step {
  id: string;
  kind: string;
  role?: string;
  model?: string;
  rounds: number;
  routes?: Route[];
}

export interface Thread {
  channel: string;
  ts: string;
}

export interface Bead {
  bead: string;
  state?: string;
  detail?: string | null;
  unit?: string;
  title?: string;
  status?: string;
  parked?: string | null;
  asked?: { step: string; question: string; at: number } | null;
  thread?: Thread | null;
  flow?: string;
  step?: string;
  steps?: Step[];
  run?: Run | null;
  command?: { step: string; round: number; since: number } | null;
  cost?: number;
  pull_request?: string | null;
  merged?: unknown;
  branch?: string | null;
  workflow?: string | null;
  runs?: string[];
  changed_at?: number;
  /** How often each step ended with each outcome, as `step:outcome`. */
  taken?: Record<string, number>;
  /** Seconds each step has taken in all. */
  spent?: Record<string, number>;
}

export interface SubGoal {
  id: string;
  title?: string;
  objective?: string;
  state?: string;
  branch?: string;
  pull_request?: string | null;
  foreman?: {
    wakes?: number;
    inbox?: number;
    failures?: number;
    retry_at?: number | null;
    waiting?: string | null;
    message?: string | null;
  };
  beads?: Bead[];
  ship?: Bead | null;
}

/** What a tier's log adds up to. */
export interface Agents {
  kinds: Record<string, number>;
  roles: Record<string, { runs: number; failed: number; cost: number; last_at: number; model: string }>;
  wakes: Record<string, number>;
  subgoals: Record<string, Record<string, number>>;
}

export interface Goal {
  id: string;
  agents?: Agents | null;
  phase: string;
  definition?: string;
  thread?: Thread | null;
  objective?: string;
  stop_when?: string;
  wakes?: number;
  failures?: number;
  retry_at?: number | null;
  inbox?: number;
  cost?: number;
  started_at?: number;
  subgoals?: SubGoal[];
  run?: Run | null;
  runs?: string[];
  evidence?: string | null;
  need?: { text: string; at: number } | null;
}

export interface Conversation {
  ts: string;
  at?: number;
  channel?: string;
  goal?: string | null;
  first?: string | null;
  last?: string;
  lines: number;
}

export interface Executive {
  agents?: Agents | null;
  wakes?: number;
  cost?: number;
  threads?: number;
  waiting?: number;
  memos?: number;
  pending?: { bead: string; as: string; once_closed: string; reason: string }[];
  retry_at?: number | null;
  run?: Run | null;
  runs?: string[];
  conversations?: Conversation[];
}

export interface Pulse {
  state: 'waiting' | 'parked' | 'working' | 'open' | 'idle';
  agents: number;
  pipelines: number;
  parked: number;
  waiting: number;
  goals: number;
  beads: number;
  done: number;
}

export interface Project {
  id: string;
  workflow?: string | null;
  executive?: Executive | null;
  goals?: Goal[];
  finished?: { id: string; phase: string; objective?: string; cost?: number; started_at?: number }[];
  loose?: Bead[];
  pulse?: Pulse;
}

export interface Action {
  label: string;
  tone: 'primary' | 'plain' | 'danger';
  action_id: string;
  value: string;
  confirm?: string;
}

export interface Edit {
  bead: string;
  acceptance_criteria?: string | null;
  priority?: number;
  defer?: string;
  reason?: string | null;
}

export interface Item {
  key: string;
  kind: 'charter' | 'intake' | 'completion' | 'request' | 'question' | 'proposal' | 'step_question' | 'parked';
  project: string;
  for_project?: string;
  goal?: string | null;
  unit?: string;
  title: string;
  summary: string;
  at?: number | null;
  sections?: { label: string; text?: string | null }[];
  edits?: Edit[];
  actions: Action[];
  reply?: (Thread & { hint?: string }) | null;
  slack?: Thread | null;
}

export interface Picture {
  release?: string | null;
  links?: { slack?: string | null; accounts?: string | null; intake?: string | null };
  inbox?: Item[];
  accounts?: Account[];
  runs?: Run[];
  projects?: Project[];
}

export interface Line {
  who: 'owner' | 'factory' | 'event';
  kind: string;
  text: string;
  at?: number | null;
}
