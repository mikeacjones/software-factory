#!/usr/bin/env python3
"""Prove that the tests a change adds or alters fail without the change.

Run in a unit's worktree:

    test-proof.py --base <commit> --out <directory>

Zig keeps a file's tests beside its code, so the change cannot be split by
file. This splits it at `test` blocks. It makes a copy of the worktree, puts
every changed file back to what it was at the base, and then puts into those
files only the test blocks the change added or altered. Each such test is run
by name on that tree, and one line of `<out>/INDEX.txt` says what happened:

    FAILS-AT-BASE     the test fails on the code as it was: it tests the change
    NO-BUILD-AT-BASE  the test does not compile on the code as it was: it needs
                      the new code, which is weaker proof than a failure
    PASSES-AT-BASE    the test passes on the code as it was: it does not test
                      the change (the flow sends this back to the builder)
    SKIPPED-AT-BASE   the test skipped itself, so nothing is known
    NOT-RUN-AT-BASE   nothing in the code as it was reaches the test's file
    NOT-JUDGED        the test is in a new file that also holds new code, or
                      there were more new tests than this tool runs
    REMOVED           a test of the base that the change takes away

The exit code is 0 whenever the tests could be judged, whatever was found: the
flow decides by the lines. It is not 0 when the tool itself could not do its
work.

The parsing relies on `zig fmt`, which the flow has already required: a test
block starts on a line `test "name" {` and ends at the first later line that
is `}` at the same indentation.
"""

import argparse
import os
import re
import shutil
import signal
import subprocess
import sys

TEST_START = re.compile(
    r'^(?P<indent>[ \t]*)test\b[ \t]*(?P<name>"(?:[^"\\]|\\.)*"|[A-Za-z_@][\w.]*)?[ \t]*\{[ \t]*(?P<closed>\}[ \t]*)?$'
)
NEW_CODE = re.compile(r"^[ \t]*(?:pub[ \t]+)?(?:export[ \t]+|inline[ \t]+|extern[ \t]+)*fn[ \t]")
PROBE = "    if (true) return error.TestProofRan;\n"
MOST_TESTS = 40
MEANING = [
    ("FAILS-AT-BASE", "the test fails on the code as it was before the change, so it tests the change."),
    (
        "NO-BUILD-AT-BASE",
        "the test does not compile on the code as it was: it uses what the change adds. That it would fail without the change is not proved.",
    ),
    (
        "PASSES-AT-BASE",
        "the test passes on the code as it was before the change, so it does not test the change. Make it assert the behaviour the change adds or corrects, so that it fails without the change, or take it out.",
    ),
    ("SKIPPED-AT-BASE", "the test skipped itself when run, so nothing is known about it."),
    ("NOT-RUN-AT-BASE", "nothing in the code as it was reaches the test's file, so it could not be run there."),
    ("NOT-JUDGED", "the test was not run on the code as it was, for the reason given."),
    ("REMOVED", "a test the code had before the change, which the change takes away."),
]
RUN_SECS = 600


class Block:
    """One `test` block of a file: its name ("" if it has none), its lines
    and where they are."""

    def __init__(self, name, start, end, text):
        self.name = name
        self.start = start
        self.end = end
        self.text = text


def test_name(raw):
    if raw is None:
        return ""
    if raw.startswith('"'):
        return raw[1:-1].replace('\\"', '"').replace("\\\\", "\\")
    return raw


def blocks(text):
    """The test blocks of a file, in order."""
    lines = text.splitlines(keepends=True)
    found = []
    at = 0
    while at < len(lines):
        match = TEST_START.match(lines[at].rstrip("\n"))
        if not match:
            at += 1
            continue
        end = at
        if not match.group("closed"):
            closing = match.group("indent") + "}"
            end = at + 1
            while end < len(lines) and lines[end].rstrip("\n").rstrip() != closing:
                end += 1
            if end == len(lines):
                # Not formatted as `zig fmt` leaves it: take no block here.
                at += 1
                continue
        found.append(Block(test_name(match.group("name")), at, end + 1, "".join(lines[at : end + 1])))
        at = end + 1
    return found


def keyed(found):
    """Blocks by name and by which of that name it is, so that a file's
    unnamed blocks are told apart by their order."""
    seen = {}
    out = {}
    for block in found:
        nth = seen.get(block.name, 0)
        seen[block.name] = nth + 1
        out[(block.name, nth)] = block
    return out


def only_tests(text):
    """Whether a file holds tests and nothing a test could be testing."""
    lines = text.splitlines(keepends=True)
    inside = set()
    for block in blocks(text):
        inside.update(range(block.start, block.end))
    rest = [line for at, line in enumerate(lines) if at not in inside]
    return not any(NEW_CODE.match(line) for line in rest)


def with_tests(base_text, head_blocks, wanted):
    """The base's file with the wanted test blocks of the head put in: over
    the block of the same name where the base has one, at the end otherwise."""
    lines = base_text.splitlines(keepends=True)
    base = keyed(blocks(base_text))
    replace = {}
    append = []
    for key in wanted:
        block = head_blocks[key]
        if key in base:
            replace[base[key].start] = (base[key].end, block.text)
        else:
            append.append(block.text)
    out = []
    at = 0
    while at < len(lines):
        if at in replace:
            end, text = replace[at]
            out.append(text)
            at = end
        else:
            out.append(lines[at])
            at += 1
    text = "".join(out)
    for block in append:
        if text and not text.endswith("\n"):
            text += "\n"
        text += "\n" + block
    return text


def without_tests(text, drop):
    """The file without the test blocks whose keys are in `drop`."""
    lines = text.splitlines(keepends=True)
    gone = set()
    for key, block in keyed(blocks(text)).items():
        if key in drop:
            gone.update(range(block.start, block.end))
    return "".join(line for at, line in enumerate(lines) if at not in gone)


def probed(text, names):
    """The file with each named test made to fail as soon as it runs."""
    lines = text.splitlines(keepends=True)
    for block in sorted(blocks(text), key=lambda b: -b.start):
        if block.name in names and block.end - block.start > 1:
            lines.insert(block.start + 1, PROBE)
    return "".join(lines)


def git(*args, check=True):
    done = subprocess.run(["git", *args], capture_output=True)
    if check and done.returncode != 0:
        raise SystemExit(f"test-proof: git {' '.join(args)} failed: {done.stderr.decode(errors='replace').strip()}")
    return done.stdout


def changes(base):
    """What the change did to each file: (status, path, is a submodule)."""
    out = []
    raw = git("diff", "--raw", "--no-renames", "-z", base, "HEAD").decode(errors="replace")
    parts = raw.split("\0")
    for at in range(0, len(parts) - 1, 2):
        meta, path = parts[at].split(), parts[at + 1]
        out.append((meta[4][0], path, "160000" in (meta[0].lstrip(":"), meta[1])))
    return out


def run_tests(tree, names):
    """Run the named tests in the tree. Returns (output, timed out)."""
    argv = ["zig", "build", "test", "--summary", "all"] + [f"-Dtest-filter={name}" for name in names]
    process = subprocess.Popen(
        argv, cwd=tree, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True
    )
    try:
        output, _ = process.communicate(timeout=RUN_SECS)
        return output.decode(errors="replace"), False
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        output, _ = process.communicate()
        return output.decode(errors="replace"), True


# The line of the build's summary for the SDK's own test binary, which is the
# one with no name: `+- run test 5 pass, 1 fail (6 total)`.
RAN = re.compile(r"^\+- run test (?P<counts>\d+ [a-z]+(?:, \d+ [a-z]+)*) \(\d+ total\)", re.M)


def failed(output, name):
    return any(
        line.startswith("error:") and (f".test.{name}'" in line or f".decltest.{name}'" in line)
        for line in output.splitlines()
    )


def compile_error_in(output, path):
    """Whether the compiler refused the test's own file. The SDK's examples
    are test binaries of their own, so the main one can run while the one
    that holds the test does not compile."""
    return re.search(rf"^{re.escape(path)}:\d+:\d+: error:", output, re.M) is not None


def copy_tree(source, tree):
    """A copy of the worktree with its build cache, without its link to git.
    A clone where the file system has them, so that the cache costs nothing."""
    clone = ["cp", "-c", "-R"] if sys.platform == "darwin" else ["cp", "-R", "--reflink=auto"]
    done = subprocess.run([*clone, source, tree], capture_output=True)
    if done.returncode != 0:
        shutil.rmtree(tree, ignore_errors=True)
        done = subprocess.run(["cp", "-R", source, tree], capture_output=True)
        if done.returncode != 0:
            raise SystemExit(f"test-proof: could not copy the worktree: {done.stderr.decode(errors='replace').strip()}")
    link = os.path.join(tree, ".git")
    if os.path.isdir(link) and not os.path.islink(link):
        shutil.rmtree(link)
    elif os.path.lexists(link):
        os.remove(link)


def write(tree, path, data):
    full = os.path.join(tree, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "wb") as file:
        file.write(data)


def main():
    parser = argparse.ArgumentParser(description=(__doc__ or "").split("\n")[0])
    parser.add_argument("--base", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)
    index = []
    notes = []

    # What the change did, file by file.
    base_text = {}
    head_blocks = {}
    wanted = {}
    judged = []  # (path, name)
    unjudged = []  # (path, name, why)
    removed = []
    base_bodies = {}
    changed = changes(args.base)
    for status, path, submodule in changed:
        if submodule:
            notes.append(f"`{path}` is a submodule the change moved: the tests ran against it as the change has it.")
            continue
        if not path.endswith(".zig"):
            continue
        if status != "A":
            base_text[path] = git("show", f"{args.base}:{path}").decode(errors="replace")
            for block in blocks(base_text[path]):
                if block.name:
                    base_bodies.setdefault(block.name, set()).add(block.text)
    head_names = set()
    for status, path, submodule in changed:
        if submodule or not path.endswith(".zig") or status == "D":
            continue
        text = git("show", f"HEAD:{path}").decode(errors="replace")
        head_blocks[path] = keyed(blocks(text))
        base = keyed(blocks(base_text[path])) if path in base_text else {}
        whole_new = status == "A"
        testable = not whole_new or only_tests(text)
        wanted[path] = []
        for key, block in head_blocks[path].items():
            name = key[0]
            if name:
                head_names.add(name)
            if key in base and base[key].text == block.text:
                continue
            if name and block.text in base_bodies.get(name, ()):
                continue  # moved from another file, unaltered
            if not testable:
                if name:
                    unjudged.append((path, name, "its file is new and also holds new code"))
                continue
            wanted[path].append(key)
            if name:
                judged.append((path, name))
    for status, path, submodule in changed:
        if path in base_text:
            for block in blocks(base_text[path]):
                if block.name and block.name not in head_names:
                    removed.append((path, block.name))
    # A test another changed file still has was moved, not removed. One that
    # an unchanged file has cannot be seen here, and is rare enough to list.

    if len(judged) > MOST_TESTS:
        for path, name in judged[MOST_TESTS:]:
            unjudged.append((path, name, f"the change has more than {MOST_TESTS} new tests"))
        judged = judged[:MOST_TESTS]

    results = {}
    if judged:
        tree = os.path.join(out, ".tree")
        shutil.rmtree(tree, ignore_errors=True)
        try:
            copy_tree(os.getcwd(), tree)
            head_text = {}
            for status, path, submodule in changed:
                if submodule:
                    continue
                if status == "A":
                    if path.endswith(".zig"):
                        if wanted.get(path):
                            head_text[path] = git("show", f"HEAD:{path}").decode(errors="replace")
                        else:
                            full = os.path.join(tree, path)
                            if os.path.lexists(full):
                                os.remove(full)
                elif not path.endswith(".zig"):
                    write(tree, path, git("show", f"{args.base}:{path}"))

            def lay(tests, probe=False):
                """Make the tree the code as it was, with the change's
                unnamed test blocks (which hold its imports) and only these
                of its named tests. A name the base does not know stops a
                whole file from compiling, so one test that needs the new
                code must not be allowed to hide another that does not."""
                for path, keys in wanted.items():
                    here = [key for key in keys if not key[0] or (path, key[0]) in tests]
                    if path in head_text:
                        drop = {key for key in keys if key not in here}
                        text = without_tests(head_text[path], drop)
                    elif path in base_text:
                        text = with_tests(base_text[path], head_blocks[path], here)
                    else:
                        continue
                    if probe:
                        text = probed(text, {name for p, name in tests if p == path})
                    write(tree, path, text.encode())
                for path, text in base_text.items():
                    if path not in wanted:
                        write(tree, path, text.encode())

            log = open(os.path.join(out, "runs.txt"), "w")
            maybe_passed = []
            for path, name in judged:
                lay({(path, name)})
                output, timed_out = run_tests(tree, [name])
                log.write(f"===== {path}: {name}\n{output}\n")
                ran = RAN.search(output)
                if timed_out:
                    results[(path, name)] = ("FAILS-AT-BASE", f"still running after {RUN_SECS} seconds")
                elif failed(output, name):
                    results[(path, name)] = ("FAILS-AT-BASE", "")
                elif ran is None or compile_error_in(output, path):
                    results[(path, name)] = ("NO-BUILD-AT-BASE", "")
                elif "skip" in ran.group("counts"):
                    results[(path, name)] = ("SKIPPED-AT-BASE", "")
                else:
                    maybe_passed.append((path, name))
            if maybe_passed:
                # Did each one run at all? Make it fail on entry and see.
                lay(set(maybe_passed), probe=True)
                output, timed_out = run_tests(tree, [name for _, name in maybe_passed])
                log.write(f"===== did they run: {', '.join(name for _, name in maybe_passed)}\n{output}\n")
                built = RAN.search(output) is not None
                for path, name in maybe_passed:
                    if failed(output, name):
                        results[(path, name)] = ("PASSES-AT-BASE", "")
                    elif built and not timed_out:
                        results[(path, name)] = ("NOT-RUN-AT-BASE", "")
                    else:
                        results[(path, name)] = ("NOT-JUDGED", "the run that checks whether it ran did not finish")
            log.close()
        finally:
            shutil.rmtree(tree, ignore_errors=True)

    for path, name in judged:
        verdict, why = results[(path, name)]
        index.append(f"{verdict} {path}: {name}" + (f" ({why})" if why else ""))
    for path, name, why in unjudged:
        index.append(f"NOT-JUDGED {path}: {name} ({why})")
    for path, name in removed:
        index.append(f"REMOVED {path}: {name}")
    count = lambda verdict: sum(1 for line in index if line.startswith(verdict + " "))
    head = [
        f"Tests the change adds or alters: {len(judged) + len(unjudged)}. "
        f"Fail at the base: {count('FAILS-AT-BASE')}. Do not build at the base: {count('NO-BUILD-AT-BASE')}. "
        f"Pass at the base: {count('PASSES-AT-BASE')}. Not known: "
        f"{count('SKIPPED-AT-BASE') + count('NOT-RUN-AT-BASE') + count('NOT-JUDGED')}. "
        f"Tests removed: {len(removed)}.",
        "",
    ]
    legend = [
        f"{verdict}: {meaning}" for verdict, meaning in MEANING if any(line.startswith(verdict + " ") for line in index)
    ]
    with open(os.path.join(out, "INDEX.txt"), "w") as file:
        file.write("\n".join(head + index + ([""] + legend if legend else []) + ([""] + notes if notes else [])) + "\n")
    sys.stdout.write("\n".join(head + index) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
