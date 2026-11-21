//! The tool the factory keeps for the Zig project that runs its tests on
//! Linux in containers, run with a stand-in for `docker`: two runs at once
//! book no more than Docker's budget between them, bookings go back however
//! a run ends, and the exit status says whose fault a failure was. No Docker.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::process::Command;

use common::world::repo_config;

#[test]
fn test_linux_shares_one_memory_ledger_and_exits_by_whose_fault() {
    let tool = repo_config().join("tools/temporal-sdk-zig/test-linux.sh");
    let test = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test-linux.sh");
    let output = Command::new("sh").arg(&test).arg(&tool).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(stdout.ends_with("all passed\n"), "{stdout}\n{stderr}");
}
