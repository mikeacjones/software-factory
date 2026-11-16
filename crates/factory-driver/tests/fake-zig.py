#!/usr/bin/env python3
"""A stand-in for `zig build test`, for the test of the Zig project's
test-proof tool. It reads the tree it is run in the way the real build would
see it, and prints what the real build prints.

  - Only `src/root.zig` and the files it imports are compiled.
  - A file that uses `added` while the root does not declare it does not
    compile, and neither does anything else.
  - Of the tests whose names contain a filter: one whose first line returns
    an error fails, one that returns `error.SkipZigTest` is skipped, one that
    expects `fixed` passes only if the root declares it true, and any other
    passes. A test with no name always runs and passes.
"""
import os
import re
import sys

filters = [arg.split("=", 1)[1] for arg in sys.argv[1:] if arg.startswith("-Dtest-filter=")]
root = open("src/root.zig").read()
files = {"src/root.zig": root}
errors = []
for name in re.findall(r'@import\("([^"]+)"\)', root):
    path = os.path.join("src", name)
    if os.path.exists(path):
        files[path] = open(path).read()
    else:
        errors.append(f'src/root.zig:1:1: error: unable to load "{name}": FileNotFound')
for path, text in files.items():
    if "use(added)" in text and "const added" not in root:
        errors.append(f"{path}:1:1: error: use of undeclared identifier 'added'")
if errors:
    print("+- run test\n   +- compile test debug native %d errors" % len(errors))
    print("\n".join(errors))
    print("error: %d compilation errors" % len(errors))
    print("Build Summary: 1/3 steps succeeded (1 failed)")
    sys.exit(1)

passed = failed = skipped = 0
for path, text in files.items():
    passed += len(re.findall(r"^test \{$", text, re.M))
    for name, first in re.findall(r'^test "([^"]*)" \{\n([^\n]*)\n', text, re.M):
        if not any(wanted in name for wanted in filters):
            continue
        if "SkipZigTest" in first:
            skipped += 1
        elif "return error." in first or ("expect(fixed)" in first and "const fixed = true;" not in root):
            failed += 1
            print(f"error: 'root.test.{name}' failed:")
        else:
            passed += 1
counts = [f"{passed} pass"]
if failed:
    counts.append(f"{failed} fail")
if skipped:
    counts.append(f"{skipped} skip")
print("Build Summary: 3/3 steps succeeded")
print(f"+- run test {', '.join(counts)} ({passed + failed + skipped} total) 5ms")
sys.exit(1 if failed else 0)
