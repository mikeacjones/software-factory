# The owner's workspace

What the dashboard is for, how it is laid out and why, and what is left before the owner can leave Slack. [architecture.md](architecture.md) states what it may and may not do; this page is the design behind it.

## What the owner comes to it for

The factory runs without anyone watching, so the owner looks in for three reasons, in this order of urgency:

1. **Something is waiting on me.** A charter to start, a completion to accept, bead changes only the owner may approve, a director's question, a review's proposal, a goal definition the executive proposed. Until 2026-11-18 these lived only in Slack threads, scattered across one channel per project, and the period report showed waits of eight hours and more on a question nobody had seen.
2. **Is anything wrong?** A pipeline parked, an account at its limit, a goal that has stopped moving.
3. **What is it doing?** Which agents run where, how far each goal has come, what a bead's pipeline has been through.

The old page answered the third well and the first two not at all. It was one long column, so a question on one project sat below every other project's beads.

## What we learned from tools that do this

- **An inbox driven by events is not a chat.** LangChain's Agent Inbox treats the owner's side of an agent system as a queue of things that need a decision, each there because something happened, not as a conversation window. That suits work that is event-driven, pauses for approval and spans hours to days, which is the factory exactly. [AgentMail on the Agent Inbox](https://www.agentmail.to/insights/langchain-agent-inbox)
- **Show what pressing the button does.** An approval that does not show the action it resolves to is not a real check by a person. Every item here shows the charter, the evidence, the beads to file or the edits, in full, next to the buttons. [Ganglani, tool approval patterns](https://www.kunalganglani.com/blog/tool-approval-patterns-ai-agents)
- **Ask only where a mistake is expensive.** Blanket approvals train people to click through. The inbox holds only what the factory itself stopped for, and nothing informational: a pipeline running, a sub-goal landing and an account filling are on the map and the floor, not in the inbox. [The Skins Factory on approval fatigue](https://www.theskinsfactory.com/uiux-design-questions-answers/how-do-you-design-human-in-the-loop-ai-without-creating-approval-fatigue)
- **Triage is keyboard-first.** Linear's triage inbox works by shortcuts shown beside every action, so they are learned by use: `j` and `k` move, `1` to `9` press the item's buttons in order, `r` replies, `⌘↵` sends, `g i`, `g f` and `g 1`… go to the inbox, the floor and a project. [Linear changelog: triage](https://linear.app/changelog/2021-06-29-linear-release-and-issue-triage)
- **Agent systems are drawn as graphs with status on the nodes.** Grafana's agent observability, Microsoft's Agent Framework dashboard and others converge on the same view: the topology, each node coloured by status, with drill-down to a run's trace. The factory has two such graphs: the tiers above the pipelines, and each pipeline's flow. [Grafana: agent workflows](https://grafana.com/docs/grafana-cloud/machine-learning/ai-observability/guides/agent-dependencies-and-workflows/), [Svelte Flow layouting](https://svelteflow.dev/learn/layouting/overview)

## The layout

```text
┌────────────┬─────────────────────────────────────────────┬──────────────┐
│ Factory    │  temporal-sdk-zig                           │  Inspector   │
│ ● Live     │  Waiting 0 · Agents 3 · Running 3 · 18/27   │  (the chosen │
│            ├─────────────────────────────────────────────┤   node:      │
│ Inbox    1 │                                             │   bead and   │
│ Floor    3 │  [Active pipelines|All|Cards]  [✦ Tidy]      │   pipeline,  │
│            │                 Exec                        │   goal and   │
│ PROJECTS   │          Goal ───────┴────── Goal           │   its talk,  │
│ ● sf     1 │      Sub-goal  Sub-goal    Sub-goal         │   executive  │
│ ○ swap-bot │   bead  ┌bead─────────┐  bead  bead         │   and its    │
│ ◉ zig    3 │   card  │build  ×1    │  card  card         │   threads)   │
│            │         │checks ×2 ┐  │                     │              │
│            │         │break ─×3─fix│           [minimap] │              │
│ ACCOUNTS   │                                             │              │
│ claude ▬▬  │                                             │              │
└────────────┴─────────────────────────────────────────────┴──────────────┘
```

- **The sidebar is the map of places.** The inbox first, with its count; the floor; then every project with a dot (amber: waiting on you, orange: something parked, pulsing blue: working, green: open but quiet, grey: idle) and a count. The accounts sit at the foot as two thin bars each, since the owner most often needs only "is anything at a limit?". The tab's title carries the inbox count, so a pinned tab says when it needs a look.
- **The inbox is a list and a reading pane.** Each item says what kind of answer it needs, which project and goal it is for, and how long it has waited. The pane holds the full question, the buttons in the order the factory posted them, a reply box when a reply is read, and the goal's whole conversation from its log, so the owner answers with the history in front of them. An answer sent dims the item until the factory's own records say it has been taken in; the item is never removed by the page alone.
- **A project is a graph, top down.** Executive, then goals, sub-goals and beads below them, laid out with dagre. Edges into anything running are animated, edges into anything waiting on the owner are amber, and sub-goals that have landed are folded to one node. Every card has a fixed size that the layout and the page share, with its text clipped to fit, so no browser's fonts can make one card overlap another. Choosing a node opens the inspector beside the graph; the choice is in the address, so it can be linked and survives a reload.
- **A bead is drawn with its whole pipeline.** A bead running, parked or asking the owner (or every bead that has started, or none: the toggle in the map's corner, remembered in the browser) is a frame holding its flow's steps. The main path, the longest chain of routes forward from the first step, runs straight down; a step off it, such as `fix`, stands beside it level with the steps that lead there, the routes into it on one rail; a route back up is dashed and runs down the right of the column; a route out of the flow (`park`, `ask`, `end`) drops to a mark on the left. Each route the bead has taken is drawn heavier with how often (from the outcomes of `step.ended` in the unit's log), each step with how often it was reached and the time spent in it, and the step it stands at is lit. "Why is this bead still going?" is the count on the dashed line from `break` back to `fix`. Every pipeline of one flow looks the same, so two can be compared at a glance.
- **A tier is drawn with its agents.** The executive, a goal and a sub-goal open, like a bead, into a frame of their agents at work, counted from the tier's own log: what woke it across the top (the director's wakes by the reason `wake.routed` gives, with names and numbers taken out so that reasons of one shape add up; the owner's messages), each agent below as a column (the director, the director agreeing a charter, the designer, a foreman, the executive) with its runs, failures, cost and model, and under each agent on a bracket what it did and how often: sub-goals opened, pipelines started, beads filed and changed, foremen and the executive told, sub-goals shipped, questions to the owner, designs proposed, accepted and sent back, results refused by code, a foreman's reports. An agent at work now is lit. A foreman's cost is not shown, since the log does not say which sub-goal a foreman's run was for.
- **Every card opens and folds.** The toggle in the map's corner sets what is open: what is active (beads running, parked or asking, and tiers whose agent is at work), everything, or nothing. Each card's ⤢ opens it and each frame's ⤡ folds it back, and that choice is kept in the browser until the corner toggle is changed.
- **The owner can arrange the map.** Any card, frame or step can be dragged. Where the owner puts things is kept in that browser for that project and survives the page's updates and a reload; **Tidy** forgets it and lays the map out again.

## Answering from the page

An answer here is the same event as the same answer in Slack (see [architecture.md](architecture.md)), so the two can be used side by side, and an item answered in Slack leaves the inbox too. Replies are posted into the Slack thread as from the dashboard, so the thread stays readable during the change.

## Before Slack can go

- **A pipeline step's question.** Its driver reads the answer from the unit's Slack thread while it waits, so the page can only show it and link there. It needs an answer the driver can read from somewhere else, such as a signal to the pipeline's workflow.
- **Opening a goal or a conversation** is done: each project's page has "Message the executive" ("Open a goal" where there is none), which `factory-engine owner <project> new` puts at the top of the project's channel as from the dashboard and hands to the tiers as the owner's message there, so the answer has a thread in Slack too. Without Slack it goes to the executive as a thread of the factory's own (`Event::Tell`), or opens a goal (`Event::Open`).
- **Being told.** Slack notifies the owner's phone. A page can show a browser notification when the inbox grows, but only while it is open; a push channel is needed for the rest.
- **The owner's identity.** The page acts as the first owner the Slack settings name. Without Slack the factory needs its own record of who the owner is.
- **Who may act from the network.** Anyone on a network given with `--act-allow` can answer as the owner. That is the same trust as the LAN the dashboard already serves, but answers steer agents that run in bypass mode, so a sign-in is due before it is opened beyond one trusted network.
