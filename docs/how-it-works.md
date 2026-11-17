# How the factory works, in diagrams

This page draws what [architecture.md](architecture.md) says in prose. It shows the factory as the code and the config are on 2026-11-16, with the Zig SDK as the project. Terms are used as the [glossary](glossary.md) defines them.

Two rules explain most of the shapes below.

- **Agents propose and code acts.** Every box marked *agent* returns a typed result. Code checks it, and code does everything that changes the world: commits, pushes, pull requests, merges, beads, Slack.
- **Nothing waits in a process.** Whatever waits, retries or remembers is a workflow on the Temporal server. The one process on the Mac, the engine's worker, does their work as activities.

## 1. The parts, and what talks to what

```mermaid
flowchart TB
    owner(["Owner"])
    slack["Slack<br/>one channel per project, one thread per goal and per run"]

    subgraph server["Temporal server, namespace software-factory"]
        intake["Intake workflow<br/>id: intake"]
        project["Project workflow<br/>id: project-ID, one per project"]
        pipeline["Pipeline workflows<br/>id: pipeline-GOAL-BEAD-N, one per unit's walk"]
        accounts["Accounts workflow<br/>id: accounts"]
        sweep["Issue sweep workflows<br/>id: issues-PROJECT-TIME, started hourly by the schedule issues-PROJECT"]
    end

    subgraph worker["The Mac: one process, the engine's worker, kept alive by launchd"]
        listen["Activity listen<br/>holds the Slack socket"]
        watch["Activity watch<br/>reads the goals' records and asks the server about each pipeline"]
        turn["Activity turn<br/>a supervisor made for one event:<br/>executive, directors, designers, foremen, maintainer"]
        steps["Activities prepare, agent, plain, finish, park<br/>the driver does one step of one unit"]
        issues["Activity issues<br/>gives open beads without one a GitHub issue, and sets type and priority"]
        claude["claude -p<br/>headless, bypass mode, one account's token"]
        records[("State directory<br/>goals, units, schedules, class checks, accounts")]
    end

    beads[("The project's bead database")]
    git[("Worktrees, branches and the remote")]
    github["GitHub<br/>pull requests, CI, issues"]

    owner <--> slack
    slack -->|"socket event"| listen
    intake -->|"keeps alive"| listen
    intake -->|"keeps alive"| watch
    listen -->|"signal: what the owner did"| project
    watch -->|"signal: a pipeline began or ended"| project
    project -->|"one event at a time"| turn
    turn -->|"pipelines to start, and when to look again"| project
    project -->|"starts as child workflows"| pipeline
    pipeline -->|"one step at a time"| steps
    pipeline <-->|"which account, and word when one is back"| accounts
    turn --> claude
    steps --> claude
    turn --> records
    steps --> records
    turn --> beads
    steps --> beads
    steps --> git
    steps --> github
    sweep -->|"one sweep"| issues
    issues --> beads
    issues --> github
    turn -->|"posts and buttons"| slack
    steps -->|"status in the run's thread"| slack
```

The dashboard (`factory-dash`) is outside this picture on purpose: it only reads the state directory.

## 2. A goal, from the owner's message to a landed pull request

Every message and button press of the owner takes the path of the first four arrows. After that the diagram draws it as one arrow.

```mermaid
sequenceDiagram
    actor Owner
    participant Slack
    participant Intake as Intake workflow
    participant Project as Project workflow
    participant Tiers as A turn of the tiers
    participant Pipe as Pipeline workflow
    participant GH as GitHub

    Owner->>Slack: top-level message in the project's channel
    Slack->>Intake: socket event, heard by listen
    Intake->>Project: signal with the event
    Project->>Tiers: activity turn, one event at a time
    Note over Tiers: Executive agent, one session per conversation.<br/>It answers, or opens a goal under a goal definition.
    Tiers->>Slack: reply, and the goal's director takes the thread
    Note over Tiers: Director agent in its intake role, one session.<br/>It agrees objective, stop condition and plan with the owner.
    Tiers->>Slack: the charter, with a Start button
    Owner->>Tiers: Start
    Note over Tiers: Code writes the agreed bead edits.<br/>The goal runs, or queues behind the project's running goals.
    Note over Tiers: Director agent, fresh session at every wake.<br/>It opens a sub-goal with its beads and paths.
    Note over Tiers: Designer agent designs the sub-goal.<br/>The director accepts it or sends it back, up to three times.
    Note over Tiers: Foreman agent, fresh session at every wake.<br/>It starts beads, side by side only on paths that do not overlap.
    Tiers-->>Project: pipelines to start, and the next time something is due

    loop each bead of the sub-goal
        Project->>Pipe: start the bead's pipeline as a child workflow
        Note over Pipe: The bead pipeline of diagram 3.<br/>It finishes absorbed, parked or failed.
        Intake->>Project: signal that the pipeline finished, seen by watch
        Project->>Tiers: activity turn
        Note over Tiers: Foreman rules on each unfixed finding: file a bead or drop it.<br/>It continues a parked pipeline, starts the next bead,<br/>or reports ready or escalate to the director.
    end

    Note over Tiers: Director ships the sub-goal.<br/>Code refuses while the default branch's CI is red,<br/>and lets one sub-goal of a project ship at a time.
    Tiers-->>Project: the ship pipeline to start
    Project->>Pipe: start the ship pipeline
    Note over Pipe: The ship pipeline of diagram 4.
    Pipe->>GH: push the branch and open one pull request
    Pipe->>GH: poll the checks, with no model
    GH-->>Pipe: every required check passed
    Pipe->>GH: squash and merge, and the issues close
    Note over Pipe: Code closes every bead the sub-goal absorbed.
    Intake->>Project: signal that the pipeline finished
    Project->>Tiers: activity turn
    Note over Tiers: Director opens the next sub-goal,<br/>or proposes that the goal is complete, with evidence.<br/>Code refuses that while a stop check fails.
    Tiers->>Slack: the evidence, with an Accept button
    Owner->>Tiers: Accept
    Note over Tiers: Code marks the goal complete, with no agent.<br/>The goal that waited longest starts.
```

## 3. One bead's pipeline

This is `config/flows/zig-sdk-bead.toml`. A flow is data. The pipeline workflow walks it one step at a time, and each step is an activity. A group of commands is one step: its commands run at the same time, and the step decides once every one has ended.

```mermaid
flowchart TD
    start(["A foreman or director starts the bead"]) --> prepare["Code prepares the unit<br/>claim the bead, worktree and branch from the sub-goal's tip, setup"]
    prepare --> build["build · agent · builder<br/>fresh session"]
    build -->|"done"| checks
    build -->|"already_done"| park

    subgraph commands["Command steps: code runs the project's commands, no model"]
        direction TB
        checks["checks · group, all at once<br/>samples-index first and alone (it edits), then fmt, readme, changelog together"] --> tests["tests · group, all at once<br/>test, and test-releasesafe only when the runtime's paths changed"]
        tests --> reports["reports<br/>writes the review reports, fails only on results named in fail_when"]
        reports --> proof["test-proof<br/>each test the change adds must fail on the code as it was"]
    end

    proof -->|"pass"| break
    commands -->|"any fails, 2 or 3 times each step; a group gives the builder every failure"| fix
    break["break · agent · tester<br/>fresh session, scratch worktree"] -->|"held"| review
    break -->|"broken, up to 3 times"| fix
    review["review · agent · reviewer<br/>fresh session, scratch worktree"] -->|"approve"| describe
    review -->|"rework, up to 2 times"| fix
    fix["fix · agent · builder<br/>continues the build session, more effort from round 2"] -->|"done, up to 8 times"| checks
    describe["describe · agent · publisher<br/>continues the build session"] -->|"ready"| absorb
    absorb["absorb · action<br/>one commit onto the sub-goal's branch"] -->|"done"| finished(["Absorbed: nothing is pushed as a pull request, and the bead stays open"])
    absorb -->|"moved: another bead got there first and merged cleanly"| recheck["The two groups again on the two together, no agent"]
    recheck -->|"pass, up to 6 times"| absorb
    recheck -->|"fail"| fix
    absorb -->|"conflict, up to 3 times"| fix
    absorb -->|"failed, up to 2 times"| describe
    park(["Park: the unit keeps its worktree, branch, commits and sessions"])
```

Not drawn, to keep it readable:

- A route that has been used as often as it allows parks the unit. Whoever runs the sub-goal's beads may continue a parked unit from a step it names, up to three times.
- Any agent step may return `needs_owner`. The question goes to the run's thread and the answer comes back to that step.
- After every step the driver pushes the unit's branch to the remote, and during a builder's run each time it commits.

## 4. Shipping a sub-goal

This is `config/flows/zig-sdk-ship.toml`. It runs once on the sub-goal's branch, which holds one commit for each bead.

```mermaid
flowchart TD
    start(["The director ships the sub-goal, and it is its turn in the project's line"]) --> sync["sync · action<br/>fetch and merge the default branch"]
    sync -->|"current or moved"| checks
    sync -->|"conflict"| fix

    subgraph commands["Command steps: the whole repository, no model"]
        direction TB
        checks["checks · group, all at once<br/>samples-index first and alone, then fmt, readme, changelog together"] --> tests["tests · group, all at once<br/>test, test-releasesafe, samples-test against a dev server, test-linux in containers"]
        tests --> reports["reports"]
    end

    reports -->|"pass"| review
    commands -->|"any fails, 2 or 3 times each step; a group gives the builder every failure"| fix
    review["review · agent · reviewer<br/>the beads read together"] -->|"approve"| describe
    review -->|"rework, up to 2 times"| fix
    fix["fix · agent · builder<br/>fresh session on the sub-goal's branch"] -->|"done, up to 6 times"| checks
    describe["describe · agent · publisher<br/>the commit message and the pull-request text"] -->|"ready"| publish
    publish["publish · action<br/>push, open one pull request"] -->|"done"| ci
    publish -->|"failed, up to 2 times"| describe
    publish -->|"conflict, up to 2 times"| fix
    ci["ci · action<br/>poll the required checks, no model"] -->|"green"| land
    ci -->|"red, up to 2 times, with every failure and its log"| fix
    land["land · action<br/>squash and merge, confirm the commit is on the default branch"] -->|"done"| landed(["Landed: code closes every bead of the sub-goal and removes the worktree"])
    land -->|"failed"| park(["Park"])
```

## 5. One round of the tester, and the fix

This is where a finding and a fix are settled by a test that code runs, and not by what an agent says.

```mermaid
sequenceDiagram
    participant Pipe as Pipeline workflow
    participant Acc as Accounts workflow
    participant Driver as Driver, in an activity
    participant Tester as Tester agent
    participant Builder as Builder agent
    participant Branch as The unit's branch

    Pipe->>Acc: which account for this step
    Acc-->>Pipe: an account's name, or wait until one is back
    Pipe->>Driver: activity agent, for step break
    Note over Driver: Scratch worktree at the unit's checkpoint.<br/>The brief holds the bead, where the reports and the test proof are,<br/>what earlier rounds found, rulings and the class keys to reuse.
    Driver->>Tester: claude -p with the brief and the result's schema
    Tester-->>Driver: outcome, findings with severity, category,<br/>reproduction command and test names, and probes
    Note over Driver: Code checks the result against the role's contract.<br/>A serious finding needs a category written as a key.
    Note over Driver: Code runs each reproduction command in the scratch worktree.<br/>It has to fail. What the tester left may only add lines.
    opt a check refuses the result
        Driver->>Tester: the reasons, in the same session
        Tester-->>Driver: a corrected result
    end
    Driver->>Branch: commit the regression tests, failing
    Note over Driver: The unit's record keeps each test's command, names and class.
    Driver-->>Pipe: broken
    Note over Pipe: The flow routes broken to fix.

    Pipe->>Acc: which account for this step
    Acc-->>Pipe: an account's name
    Pipe->>Driver: activity agent, for step fix
    Driver->>Builder: the findings and their commands, in the build session
    Builder->>Branch: commits as it goes, pushed while it works
    Builder-->>Driver: done, with evidence for each criterion
    Note over Driver: Code runs every regression test the unit has gathered.<br/>Each has to pass, and none may have left the code.
    opt one still fails
        Driver->>Builder: which one, in the same session
        Builder-->>Driver: a new result
    end
    Driver-->>Pipe: done
    Note over Pipe: Every command step runs again, then a fresh tester,<br/>told what earlier rounds found.
```

## 6. From a finding to a check that every later bead runs

Owner's decision, 2026-11-16: this loop closes with nobody in it, and what can be a tool is not left as prose. Boxes marked *code* involve no model. The dashed boxes are not built yet.

```mermaid
flowchart TD
    finding["Tester proves a finding<br/>a failing regression test and a class key"] --> record["Code: the unit's record keeps the test with its class"]
    record --> count["Code: finding_classes counts the beads each class has broken"]

    count --> checked{"Code: has the class a check that landed<br/>and has it broken no bead since?"}
    checked -->|"no"| told["The next builder's first brief lists the class<br/>words, the stopgap"]
    checked -->|"yes"| quiet["The class is left out of the brief<br/>the check runs on the change instead"]

    count --> calls{"Code: has a class broken when_class_breaks beads<br/>more than a run was last shown?"}
    calls -->|"yes"| run
    clock["Code: the schedule's hour and interval"] --> run
    run["Maintenance run<br/>agent: maintainer, fresh session"]
    ruling["A foreman or director ruling on an unfixed finding<br/>agent"] --> files

    run --> files["Files beads, each with acceptance criteria<br/>closes_class names the class a bead's check closes"]
    files --> answer["Code: checks.json keeps which bead answers the class<br/>one bead at a time for a class"]
    files -->|"not marked to build"| backlog["Code: the schedule's backlog<br/>shown to the next run"]
    backlog --> run
    files -->|"marked to build, the schedule has build_under,<br/>and no maintenance goal is building"| open["Code opens a maintenance goal<br/>writes the charter, starts it with no Start,<br/>in line with the project's other goals"]
    open --> goal["The goal runs as any goal<br/>director, designer, foreman, bead pipelines, ship pipeline"]
    goal --> lands["The pull request lands and code closes the beads"]
    lands --> landed["Code: the class is recorded as checked,<br/>at the count of beads it had broken"]
    lands --> complete["Code: every bead of the goal is closed,<br/>so the goal is complete with no agent and no Accept"]
    landed --> checked
    complete -->|"a later run may build again"| run

    lint["Not built, sf-gl5.13:<br/>one lint command in every bead pipeline,<br/>which a project's rules are added behind"]
    proofcase["Not built, sf-gl5.12:<br/>a command proves the check fails on each broken case<br/>of its class and passes on each fixed one"]
    goal -.-> lint
    goal -.-> proofcase
    classDef missing stroke-dasharray: 5 5
    class lint,proofcase missing
```

Until the lint command exists, a check the loop builds gates later beads only if it lands inside a command the bead flow already runs, such as `zig build test`.

## 7. A class of defect calls a maintenance run

The same loop in time, on the engine. `upkeep` is the Zig project's scheduled definition, and it builds under `parity`.

```mermaid
sequenceDiagram
    participant Project as Project workflow
    participant Sup as Supervisor, in a turn
    participant Maint as Maintainer agent
    participant Tracker as Bead database
    participant Slack
    participant Dir as Director, designer and foreman agents
    participant Pipe as Pipeline workflows

    Note over Project: Each turn says when something is next due.<br/>A class that has broken enough beads makes that now.
    Project->>Sup: activity turn, because a time came due
    Note over Sup: Code: run_schedule. Due by the clock, or called by a class.<br/>May it build: the schedule names build_under,<br/>and no maintenance goal is still building.
    Sup->>Maint: the charter, the measures with each class's check,<br/>the default branch's CI, the backlog, the last run's journal
    Maint-->>Sup: beads to file, which to build, a plan, paths,<br/>a digest and a journal
    Note over Sup: Code checks the result. A start, ship or close is refused.<br/>So is a build while a goal is building. Refusals go back to the same session.
    Sup->>Tracker: file the beads, each with a GitHub issue
    Note over Sup: Code keeps which bead answers which class.
    Sup->>Slack: the digest, in the project's channel
    Note over Sup: Code opens the maintenance goal in that message's thread,<br/>with a charter written from the result. Nobody presses Start.
    Sup->>Dir: the director is woken, and told how the goal came to be
    Dir-->>Sup: open a sub-goal, accept its design, start its beads
    Sup-->>Project: pipelines to start
    Project->>Pipe: child workflows
    Note over Pipe: Bead pipelines, then the ship pipeline.<br/>Landing closes the beads and records the class as checked.
    Pipe-->>Project: the ship pipeline finished, signalled by the watcher
    Project->>Sup: activity turn
    Note over Sup: Code: none of the goal's beads is open any more.<br/>The goal is complete. No director is woken and nobody accepts.
    Sup->>Slack: goal complete, in the thread
    Note over Sup: From the next bead on, builders are not told of the class.<br/>If it breaks a bead again, they are, and a run is called.
```

## Where each diagram comes from

| Diagram | Code and config |
|---|---|
| 1 | `crates/factory-engine/src/tiers.rs`, `engine.rs`, `accounts.rs`, `steps.rs`, `worker.rs` |
| 2 | `crates/factory-driver/src/goal.rs` (`Supervisor::turn`, `wake`, `wake_foreman`, `ship`, `act`), `config/goals/temporal-sdk-zig/parity.toml`, `config/roles/` |
| 3, 4 | `config/flows/zig-sdk-bead.toml`, `config/flows/zig-sdk-ship.toml`, `config/projects/temporal-sdk-zig.toml` |
| 5 | `crates/factory-driver/src/lib.rs` (`agent_step`, `verify_reproductions`, `adopt_regressions`, `check_regressions`), `crates/factory-brief/src/lib.rs` (`result_problems`) |
| 6, 7 | `crates/factory-driver/src/goal.rs` (`run_schedule`, `classes_calling`, `maintenance_problems`, `open_maintenance`, `settle_maintenance`), `crates/factory-driver/src/lib.rs` (`finding_classes`, `answer_class`, `check_landed`, `classes_unchecked`), `config/goals/temporal-sdk-zig/upkeep.toml` |

A diagram that no longer matches these is wrong: change it with the code.
