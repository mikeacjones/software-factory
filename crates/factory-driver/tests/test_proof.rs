//! The tool the factory keeps for the Zig project that proves a change's new
//! tests fail without the change, run on a small repository with a stand-in
//! for `zig build test`. No Zig and no network.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::world::repo_config;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A directory of the test's own, removed when the test ends.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Temp {
        let dir = std::env::temp_dir().join(format!("factory-test-proof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Temp(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(dir: &Path, path: &str, text: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

const BASE_ROOT: &str = r#"const fixed = false;

pub fn f() void {}

test {
    _ = @import("old_test.zig");
}

test "kept" {
    try expect(true);
}

test "altered" {
    try expect(true);
}

test "dropped" {
    try expect(true);
}
"#;

// The change: `fixed` becomes true and `added` is new. Its tests sit among
// the code, as Zig's do.
const HEAD_ROOT: &str = r#"const fixed = true;
const added = 1;

pub fn f() void {}

test {
    _ = @import("old_test.zig");
    _ = @import("new_test.zig");
}

test "kept" {
    try expect(true);
}

test "altered" {
    try expect(fixed);
    try expect(true);
}

test "proves the change" {
    try expect(fixed);
}

test "needs the new code" {
    try use(added);
}

test "tests nothing" {
    try expect(true);
}

test "skips itself" {
    return error.SkipZigTest;
}
"#;

#[test]
fn each_new_zig_test_is_run_alone_on_the_code_as_it_was_and_classed_by_what_happened() {
    let temp = Temp::new();
    let repo = temp.0.join("repo");
    let bin = temp.0.join("bin");
    let out = temp.0.join("out");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-zig.py");
    std::fs::copy(fake, bin.join("zig")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("zig"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    write(&repo, "src/root.zig", BASE_ROOT);
    write(
        &repo,
        "src/old_test.zig",
        "test \"old\" {\n    try expect(true);\n}\n",
    );
    write(&repo, "notes.txt", "as it was\n");
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    write(&repo, "src/root.zig", HEAD_ROOT);
    // A new file of tests the root now imports, one nothing imports, and a
    // new module that brings its own tests.
    write(
        &repo,
        "src/new_test.zig",
        "test \"in a new file\" {\n    try expect(fixed);\n}\n",
    );
    write(
        &repo,
        "src/orphan_test.zig",
        "test \"nothing reaches\" {\n    try expect(fixed);\n}\n",
    );
    write(
        &repo,
        "src/module.zig",
        "pub fn g() void {}\n\ntest \"of a new module\" {\n    try expect(true);\n}\n",
    );
    write(&repo, "notes.txt", "as the change has it\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "change"]);

    let tool = repo_config().join("tools/temporal-sdk-zig/test-proof.py");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new("python3")
        .arg(&tool)
        .args(["--base", &base, "--out"])
        .arg(&out)
        .current_dir(&repo)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let index = std::fs::read_to_string(out.join("INDEX.txt")).unwrap();
    let runs = std::fs::read_to_string(out.join("runs.txt")).unwrap_or_default();
    let mut lines: Vec<&str> = index
        .lines()
        .skip(2)
        .take_while(|line| !line.is_empty())
        .collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "FAILS-AT-BASE src/new_test.zig: in a new file",
            "FAILS-AT-BASE src/root.zig: altered",
            "FAILS-AT-BASE src/root.zig: proves the change",
            "NO-BUILD-AT-BASE src/root.zig: needs the new code",
            "NOT-JUDGED src/module.zig: of a new module (its file is new and also holds new code)",
            "NOT-RUN-AT-BASE src/orphan_test.zig: nothing reaches",
            "PASSES-AT-BASE src/root.zig: tests nothing",
            "REMOVED src/root.zig: dropped",
            "SKIPPED-AT-BASE src/root.zig: skips itself",
        ],
        "{index}\n{runs}"
    );
    assert_eq!(
        index.lines().next().unwrap(),
        "Tests the change adds or alters: 8. Fail at the base: 3. Do not build at the base: 1. Pass at the base: 1. Not known: 3. Tests removed: 1."
    );
    // The builder is told what the verdict that sends the change back asks.
    assert!(
        index.contains("PASSES-AT-BASE: the test passes on the code as it was before the change, so it does not test the change."),
        "{index}"
    );
    // The copy it worked in is gone, and the worktree is as it was.
    assert!(!out.join(".tree").exists());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/root.zig")).unwrap(),
        HEAD_ROOT
    );
}
