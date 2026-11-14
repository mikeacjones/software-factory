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

## session

One Claude Code conversation, named by its session ID. An agent step starts a fresh session or continues the session of a named earlier step.

## goal

An objective the owner wants reached on a project, with the condition that ends it. It starts as a conversation in Slack, runs on events, and ends when the owner accepts that the condition is met.

## pipeline

What one bead goes through from being started to being landed: a unit walking its flow. A goal runs pipelines, up to its cap at a time.

## director

The agent that directs a goal: it decides which bead's pipeline runs next, what to file, when the owner is needed and when to propose that the goal is complete. It starts fresh at every event and its decisions are checked by code.

## tick

One pass of the driver: preflight, observe, step every unit, execute actions with at most one landing, dispatch, write the heartbeat.

## gate

The proof of a milestone: a manifest of checks, each a command with a tier, a timeout and the claim it proves. A gate passes only if every check passed and no tier was skipped.
