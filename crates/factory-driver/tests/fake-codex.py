#!/usr/bin/env python3
"""A scripted stand-in for the `codex` CLI, for the driver's tests.

It takes the arguments the factory passes to `codex exec`, has the scripted
stand-in for `claude` beside it play the role, and prints that run as the
stream `codex exec --json` prints. Each call is one line of
`state/codex-calls`: the arguments, joined by spaces.

Like the real tool, the usage it reports is the session's running total.
"""
import json
import os
import subprocess
import sys

here = os.path.dirname(os.path.abspath(__file__))
state = os.path.join(here, "state")
os.makedirs(state, exist_ok=True)
argv = sys.argv[1:]
with open(os.path.join(state, "codex-calls"), "a") as calls:
    calls.write(" ".join(argv) + "\n")
if argv[:1] != ["exec"] or argv[-1:] != ["-"] or "--json" not in argv:
    sys.exit("fake codex: not a headless run: " + " ".join(argv))

resume = argv[2] if argv[1:2] == ["resume"] else ""
model = argv[argv.index("--model") + 1]
schema = argv[argv.index("--output-schema") + 1]
json.load(open(schema))
brief = sys.stdin.read()

# A fresh session is told its role at the top of its briefing. A resumed
# one is not: the stand-in's session names carry the role.
if resume:
    role = resume.split("-")[1]
    system = {"builder": "You are the builder", "tester": "You are the adversarial tester",
              "reviewer": "You are the reviewer"}.get(role, "")
else:
    system, _, brief = brief.partition("# Your briefing\n\n")
    if not brief:
        sys.exit("fake codex: a fresh session was given no role")

command = [os.path.join(here, "claude"), "--model", model, "--append-system-prompt", system]
if resume:
    command += ["--resume", resume]
ran = subprocess.run(command, input=brief, capture_output=True, text=True,
                     env={**os.environ, "CLAUDE_CODE_OAUTH_TOKEN": "codex"})
session, result = resume, None
for line in ran.stdout.splitlines():
    event = json.loads(line)
    if event.get("type") == "system":
        session = event["session_id"]
    if event.get("type") == "result":
        result = event

total_file = os.path.join(state, "codex-total-" + session)
runs = int(open(total_file).read()) + 1 if os.path.exists(total_file) else 1
open(total_file, "w").write(str(runs))


def say(event):
    print(json.dumps(event), flush=True)


say({"type": "thread.started", "thread_id": session})
say({"type": "item.completed", "item": {"id": "item_0", "type": "error",
                                        "message": "Codex is ignoring 1 unrecognized configuration setting."}})
say({"type": "turn.started"})
say({"type": "item.started", "item": {"id": "item_1", "type": "command_execution",
                                      "command": "/bin/zsh -lc 'git status'", "status": "in_progress"}})
say({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution",
                                        "command": "/bin/zsh -lc 'git status'", "exit_code": 0}})
if result is None or result.get("is_error"):
    say({"type": "turn.failed", "error": {"message": "the stand-in for claude failed"}})
    sys.exit(1)
say({"type": "item.completed", "item": {"id": "item_2", "type": "agent_message",
                                        "text": json.dumps(result["structured_output"], indent=2)}})
say({"type": "turn.completed", "usage": {
    "input_tokens": 3000 * runs, "cached_input_tokens": 1000 * runs,
    "cache_write_input_tokens": 0, "output_tokens": 100 * runs, "reasoning_output_tokens": 0}})
