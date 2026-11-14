# Architecture

The software factory takes beads from a project's backlog and turns each one into a merged pull request, without anyone watching. This page states the decisions already made. Terms in *italics* are defined in the [glossary](glossary.md).

## Who owns what

- **Beads own what the work is.** The factory reads work from each *project*'s bead database and never keeps its own copy of a *bead*.
- **Redis owns how the factory is processing it.** *Unit* records, *run* records, leases, pauses, questions and the audit log live in Redis. Jobs, results and events travel on Redis Streams.
- **Git and GitHub own code and CI.** A branch, a pull request and its checks are read from git and GitHub each time, not remembered.

The driver is level-triggered. Every *tick* it rebuilds what it believes from Redis, beads, git and GitHub, so a lost message or a change the owner made by hand cannot leave it stuck.

## Agents propose and code acts

An agent returns a typed *result*. Code checks it and code carries out every action: committing, pushing, opening a pull request, landing, closing a bead, posting to Slack.

Agents never talk to each other. A *job* message goes in and a *result* message comes out. An agent *step* has no memory beyond the *session* its *flow* tells it to continue.

A project's *flow* decides which *steps* run, in what order and on which model. Failure handling, the preconditions for landing and the invariants are code. No flow can change them.

## No agent is sandboxed

Owner's decision, 2026-11-14. Every agent runs in bypass mode as the owner's account on this Mac. There are no separate macOS accounts, no deny rules and no per-agent restrictions: the machine is the sandbox. Three things follow.

1. **Output is checked after the run**, since nothing restricts an agent while it runs. Each step's changes are compared with the path scopes its flow allows, and its typed result is validated against the diff and the bead.
2. **Files the factory depends on are checked by content hash.** Config, flows and *artifacts* can all be edited by an agent, so a change is detected, never trusted. A unit runs on a pinned copy of its flow.
3. **What the machine can reach is set by the credentials stored on it**, so those are kept narrow. The GitHub credential is a fine-grained token limited to the repositories the factory works on.

## Components

| Component | What it does | Crate |
|---|---|---|
| Shared types | Identifiers, the message envelope, failure classes | `factory-core` |
| Clock | Real and virtual time; every time-dependent rule takes a clock | `factory-clock` |
| Config | Factory settings and the project registry, as typed TOML | `factory-config` |
| Flow | The flow schema, its validator, and the flow engine: `step(flow, state, observation)` returns the next state and a list of actions, with no I/O | `factory-flow` |
| State | Every Redis key and stream, typed records, leases, pauses, the artifact store, the audit log | `factory-state` |
| Supervision | Spawn a child, stream its output, judge it alive or stalled; the *slot* runner daemon; the reaper that kills a slot's whole process tree | `factory-supervise` |
| Claude adapter | The worker command line, the stream parser, how a run ended, the *account* tracker, and a fake `claude` for tests | `factory-claude` |
| Beads adapter | Typed wrapper over `bd`, always with an explicit database; the readiness check | `factory-beads` |
| Git adapter | Worktrees, checkpoints, branches, pushes, with timeouts | `factory-git` |
| GitHub adapter | Pull requests, CI watching, the serial lander, issue sync, behind a write allowlist | `factory-github` |
| Checks | Project commands in a slot, diff policy, the weakened-tests detector, result validation | `factory-checks` |
| Slack | Questions, alerts, the digest and owner commands | `factory-slack` |
| Tasks | Scheduled and manual tasks that run beside the flow | `factory-tasks` |
| Briefings | The brief for each agent step, and the role prompts | `factory-brief` |
| Driver | The reconciler: observe, step every unit, act, land at most one, dispatch | `factory-driver` |
| Watchdog | A separate process that checks the driver, Redis, slots and overdue tasks, and alerts on its own | `factory-watchdog` |
| CLI | The `factory` binary | `factory-cli` |
| Dev tooling | `cargo xtask`: the quality command and the gate runner | `xtask` |

A crate is added by the bead that first needs it. Until then the name above is reserved.

Workers are headless: `claude -p --output-format stream-json`, in bypass mode, with the account supplied as a token in the environment. Liveness comes from what the operating system and the child's output already provide (process exit, lines on stdout, a lease the runner refreshes, CPU time), never from asking the model.

## The first cut

The first milestone walks one bead to a pull request by hand: `factory run <project> <bead>`. It is a thin slice through the crates above, not a separate program. What it leaves out is listed here so nobody mistakes it for the whole design.

- **No Redis.** A unit's state is one JSON file and its audit log one JSONL file, both under the state directory. The driver walks a single unit in the foreground; there are no ticks, slots or leases.
- **No stall rules.** A run is judged by its exit, its output stream and a wall-clock cap.
- **Accounts are balanced by their windows.** Capacity left in a window when it resets is lost, so each run uses the account whose week resets soonest, and between accounts whose weeks end together, the one whose five-hour session resets soonest. An account at its session or weekly limit is left until that window resets, a limit on one model leaves the account usable for others, and a run refused at a limit continues its session under another account. When every account is at a limit the step, or the director, waits for the earliest reset. Nothing is polled: every run's rate-limit event reports both windows, and an account the factory has not heard from for half an hour is asked about once, through the subscription's usage endpoint, with no model call.
- **Landing is one unit at a time.** After the pull request is opened the factory watches its checks by polling, with no model involved. One failed check ends the wait: its log goes back to the builder's session, and the fix goes through every step again. When every required check has passed, the factory merges the pull request, confirms the merge commit is on the default branch, closes the bead and removes the worktree, without asking any agent. A pull request that conflicts with the default branch, or whose head is not the commit the factory published, parks the unit. There is no landing queue yet, because there is only one unit.
- **Steps run one at a time.** A flow has no groups yet.

The first flow, `build-break-review`, has four roles. The **builder** implements the bead, and its session is continued for every fix. The **tester** is adversarial: it gets a scratch worktree at the checkpoint and tries to break the change by running it, and every finding must carry a reproduction it ran. The **reviewer** reads the change. The **publisher** continues the builder's session to write the commit message and pull-request text. Slack is one channel per project and one thread per run; the owner steers by replying in the thread.

A flow can also carry reports. A command step may say it *produces* a named artifact, such as a directory of scanner output, and an agent step may say it *needs* one. The factory gives the command a directory outside the worktree, and briefs each step that needs the artifact with where it is and what its index says. The validator refuses a flow in which, on some path, a step would be given reports that are missing or were written before the code last changed.

## Goals

A pipeline takes one bead to its landing. A *goal* sits above pipelines: an objective and a stop condition on a *project*, the *director* that runs it, the *flow* its beads go through and how many pipelines it may have at once. How goals of a kind run is a file, `goals/<project>/<id>.toml`; the objective itself comes from the owner.

- **It starts as a conversation.** The owner's top-level message in the project's channel opens the goal, and that message's thread is the goal's. The director talks with the owner in one session until it can propose a charter: objective, stop condition, plan, and any bead changes the owner agreed to. Nothing is written or run until the owner presses Start.
- **It runs on events.** A pipeline ended, the owner wrote in the thread, the owner answered a request: each wakes the director, in a fresh session briefed by code with the charter, what happened, the tracker, its own journal for the goal and the project's memory. Between events no model runs.
- **The director decides and code acts.** Its result is a list of decisions: start a bead's pipeline, file a bead with a type, a priority and whether it is deferred, change a bead that exists, ask the owner, propose completion. Code refuses a start on a bead that is not launchable or past the cap, completion without evidence, and any result that would leave the goal with nothing to move it. What the director may do alone is the goal definition's `[permissions]`: by default it files new beads freely and a change to an existing bead is put to the owner.
- **Findings are the director's to rule on.** When a pipeline ends, what its tester and reviewer left unfixed goes to the director, which files each as a bead or drops it with a reason.
- **Ending is the owner's act.** The director proposes completion with evidence; the factory posts it with an Accept button. Accept marks the goal complete with no agent involved. A reply instead is guidance, and the goal goes on.

The service is `factory serve`. It listens to Slack over a socket (Socket Mode), so nothing is polled and nothing needs to be reachable from outside. Pipelines run on threads of the service, each with its own Slack thread, and a restart continues the goals and pipelines it finds on disk. For now the director runs inside the service's one loop, so two goals take turns thinking, and a goal's cap above one waits on conflict handling.

## Failure classes

Every failure the factory must survive is listed in a catalogue with how code detects it, its class, the response and the resume condition. There are three classes.

- **Unit**: one unit is affected. It is retried, sent back a step, or parked. Everything else carries on.
- **Factory-wide**: nothing can usefully be dispatched (both accounts limited, Redis down, GitHub down). The driver pauses, keeps observing and resumes by itself.
- **Needs-owner**: only the owner can fix it. The scope it affects is paused and the owner is told once.

## Proof

A milestone is proved by its *gate* and by nothing else: `cargo xtask gate <id>` runs the checks in `gates/<id>.toml` and exits 0 only if every one passed. Each acceptance line of a bead is a check in its epic's gate. Checks have one of four tiers.

- **hermetic**: no network and no model. Runs anywhere.
- **host**: needs this Mac's real services, launchd jobs or worker slots.
- **live**: makes real Claude calls, on Haiku.
- **owner**: needs something only the owner can supply.

A skipped tier makes a gate incomplete, and an incomplete gate cannot close a milestone.

## Build order

Deterministic tools first (foundations, host proofs, state, supervision, the Claude adapter, work sources, GitHub, checks, Slack, tasks, operations), then the driver that composes them, then the parts where a model's behaviour matters (briefings and prompts, end-to-end runs, agent tasks, the first real project). `bd ready` gives the next bead.

## Repository layout

| Path | Holds |
|---|---|
| `crates/` | The factory's crates |
| `xtask/` | Dev tooling |
| `gates/` | Gate manifests, the report schema, and self-test fixtures |
| `proofs/` | Scripts that prove an assumption about this host |
| `docs/` | This page, the glossary, and later the runbook |
