# Glossary

One definition per term. `cargo xtask docs` fails if a term is defined twice or a required term is missing. See [architecture](architecture.md) for how the parts fit together.

## project

A repository the factory works on, described by one record in the registry: its repo path, bead database, GitHub repository, code areas, path scopes, named commands, flow and Slack channel.

## bead

An issue in a project's bead database. It defines one piece of work. The factory reads beads and never keeps its own copy.

## unit

The factory's processing of one bead: one bead, one branch, one worktree, one pull request. Its record holds the pinned flow, the current step, the count used on each backward transition and the session of each step.

## flow

A file that says what a bead goes through in one project, from being chosen to being closed: how the bead is selected, the steps, and where each outcome leads. Flows live in the factory's config directory, and a unit runs on a copy pinned by content hash.

## step

One stage of a flow. There are four kinds: an agent step (a model run in a role), a command step (a named project command with no model), a group (steps with no order among them, joined when all finish) and a factory action (push, open pull request, await CI, land, close bead).

## role

What an agent step is asked to be, such as implementer or reviewer: a system prompt plus a contract that fixes the shape of the typed result and the outcomes the step may return.

## outcome

The value a step ends with. The flow routes each outcome to another step, to park, or to a question for the owner. An outcome that leads backwards can carry feedback for the step it leads to.

## artifact

A document one step hands to later steps, such as a requirements document, a tool report or review findings. It is a file named by its content hash, kept outside the worktree, and every read checks the hash.

## run

One execution of one step: a process started by a slot's runner, with a run ID, a lease, a log and exactly one way of ending.

## job

The message that asks a slot's runner to start a run. Job kinds are `exec` (any command) and `claude` (a worker session).

## result

The message a runner publishes when a run ends: how it ended, and for an agent step the session ID, account, usage and typed result. The typed result is the JSON the agent returned, matching its role's schema.

## slot

A lane that executes one run at a time, with its own runner and work directory. It is not a security boundary.

## account

A Claude subscription the factory may run workers under. It is supplied to a run as a token in the environment, and its limits are tracked from the rate-limit event of every run.

Under the flow engine the accounts are handed out by one workflow, the *accounts workflow*. A step asks it which account to run under and is given a name, or told to wait; it releases everything waiting as soon as an account is back, whether its limit ended or the owner said to look again (`factory-engine accounts recheck`). The token for a name is read by the worker that runs the step and is in no workflow's history.

## session

One Claude Code conversation, named by its session ID. An agent step starts a fresh session or continues the session of a named earlier step.

## goal

An objective the owner wants reached on a project, with the condition that ends it. It starts as a conversation in Slack, runs on events, and ends when the owner accepts that the condition is met.

## sub-goal

A part of a goal that lands as one pull request, such as one feature. It has a branch of its own that its beads are absorbed into, one commit each, and it is shipped once: verified as a whole, opened as a pull request, and squashed onto the default branch, which closes its beads.

## pipeline

What one bead goes through from being started to being landed: a unit walking its flow. A goal runs pipelines, up to its cap at a time.

## director

The agent that directs a goal: it decides which bead's pipeline runs next, what to file, when the owner is needed and when to propose that the goal is complete. It starts fresh at every event and its decisions are checked by code. On a goal whose sub-goals have foremen it opens and ships sub-goals, and leaves their beads to them.

## executive

The agent the owner talks to in a project's channel, above every goal's director. It answers, or opens a goal under one of the project's goal definitions and hands the thread to that goal's director, and it decides which waiting goals may start beside the ones running. It is the tier of a project.

It is also where tracker work that is no goal's goes. A director that finds an open bead needing no work, because what landed delivers it or it duplicates another, passes it up to the executive, not to the owner. The executive can close such a bead, and its issue with it, at once or as soon as a bead it names has landed; code refuses one a goal is working on.

## foreman

The agent that runs one sub-goal under the director: it starts the sub-goal's beads, rules on their findings, and reports to the director when the sub-goal is ready to ship or when it needs an answer. Like the director it starts fresh at every event and its decisions are checked by code.

## ruling

A decision about a bead by whoever runs its sub-goal, made when they continue its pipeline: what is out of scope, or how a question of design is settled. It is kept with the unit and shown to every agent step from then on, the tester and the reviewer included, which a continue's note is not.

## tier

One level of the agents that run a goal, defined by a model, an effort, a time limit and its slots: the most pieces of work it may have running below it at once. The executive is the tier of a project, the director the tier of a goal and the foreman the tier of a sub-goal. Each is woken by events from the level below, and a slot count is a maximum, never a target.

## paths

The files and directories a foreman sets aside for a bead that runs beside others of its sub-goal. Beads may run at the same time only when each has paths and no two overlap, which code checks before anything starts. A director does the same for sub-goals. Files a goal definition lists as shared, which nearly every bead adds to, never count as overlap.

## release

One build of the factory and of the engine together with the config that build reads, kept in a directory of its own. launchd runs the worker from the current release, which a link names; a deploy makes a new release and moves the link.

## worker

The one process of the factory on a machine: it does the work of the workflows on the Temporal server, which are where everything waits. (Before the engine, "worker" also meant an agent's session; a role's run is now called a run.)

## turn

One thing that happened to a project, dealt with by its tiers: what the owner wrote or pressed, a pipeline that has its unit or has ended, or a time that came due. A project's workflow takes its turns one at a time, each as one activity, in which the executive, the directors and the foremen concerned read the goals' records, decide, and leave the records as they decided.

## tick

One pass of the driver: preflight, observe, step every unit, execute actions with at most one landing, dispatch, write the heartbeat.

## gate

The proof of a milestone: a manifest of checks, each a command with a tier, a timeout and the claim it proves. A gate passes only if every check passed and no tier was skipped.

## finding class

The kind of mistake a tester's finding is, as a short key such as `error-path-cleanup`. Every critical or major finding has one. They are counted across a project's units, and the most frequent are told to the next builder.

## test proof

A command step that runs each test a change adds or alters on the code as it was at the unit's base. A test that fails there tests the change; one that passes there goes back to the builder; one that cannot be compiled or reached there is reported as not proved.

## stop check

A command of the project that a goal definition names to decide the goal's stop condition. Code runs it on the default branch, shows the director what it said, and refuses a proposal that the goal is complete while it fails.
