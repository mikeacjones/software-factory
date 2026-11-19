//! `factory goal enable`, run as the owner runs it: the real `factory`
//! binary on a config directory of its own, with a stand-in for the
//! engine's command and beads kept in files. No model and no network.

// Test code: a failed setup step should stop the test where it happens.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../../factory-driver/tests/common/world.rs"]
mod world;

use std::path::Path;
use std::process::{Command, Output};

use factory_beads::Source;
use factory_config::goal_definition_hash;
use factory_driver::goal::Supervisor;
use world::World;

/// A definition of the toy project the owner may open goals under.
const FIXES: &str = "id = \"fixes\"\nmax_pipelines = 2\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";

/// The world of the driver's tests, with a second project, `factory`: the
/// factory's own, with a `[self_deploy]` table, its beads in files, and
/// its own goal definitions forbidden to its steps. The toy project has a
/// default definition in the release. With `engine`, the factory has an
/// engine whose command is a stand-in that records how it was called, and
/// fails while `engine-fails` exists.
fn factory_world(name: &str, engine: bool) -> World {
    let world = World::new(name, "true");
    std::fs::create_dir_all(world.path("factory-beads")).unwrap();
    let toy = std::fs::read_to_string(world.path("config/projects/toy.toml")).unwrap();
    let own = toy
        .replace("id = \"toy\"", "id = \"factory\"")
        .replace(
            &world.path("beads").display().to_string(),
            &world.path("factory-beads").display().to_string(),
        )
        .replace(
            "forbidden_paths = [\".github/**\"]",
            "forbidden_paths = [\".github/**\", \"config/goals/factory/**\"]",
        );
    std::fs::write(
        world.path("config/projects/factory.toml"),
        format!(
            "{own}\n[commands.release]\nargv = [\"true\"]\ntimeout_secs = 30\n\n[github]\nrepo = \"owner/factory\"\nwrite = [\"owner/factory\"]\n\n[self_deploy]\nbuild = \"release\"\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(world.path("config/goals/toy")).unwrap();
    std::fs::write(
        world.path("config/goals/toy/main.toml"),
        "id = \"main\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    if engine {
        let stand_in = world.path("bin/factory-engine");
        std::fs::write(
            &stand_in,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{calls}'\nif [ -e '{fails}' ]; then echo 'no server' >&2; exit 1; fi\necho \"the tiers of project \\`$2\\` are running, under \\`$3\\` among the rest\"\n",
                calls = world.path("engine-calls").display(),
                fails = world.path("engine-fails").display(),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        let factory = world.path("config/factory.toml");
        let text = std::fs::read_to_string(&factory).unwrap();
        std::fs::write(
            &factory,
            format!(
                "{text}\n[temporal]\naddress = \"http://127.0.0.1:1\"\nnamespace = \"default\"\ntiers = true\nengine_bin = \"{}\"\n",
                stand_in.display()
            ),
        )
        .unwrap();
    }
    world
}

fn factory(world: &World, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_factory"))
        .args(args)
        .arg("--config")
        .arg(world.path("config"))
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write(world: &World, name: &str, text: &str) -> String {
    let path = world.path(name);
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

/// Every file under a directory, or none if it is not there.
fn files_under(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path.display().to_string());
        }
    }
    found
}

#[test]
fn an_enabled_definition_is_listed_and_taken_up_without_a_restart_and_a_bead_is_filed() {
    let world = factory_world("cli-enable", true);
    std::fs::create_dir_all(world.path("bin/state")).unwrap();
    std::fs::write(
        world.path("bin/state/intake-1.json"),
        r#"{"reply":"Here is the charter.","ready":true,"objective":"Fix the crash.","stop_when":"It no longer crashes.","plan":"One bead.","edits":[]}"#,
    )
    .unwrap();
    // The service is already running when the owner enables the
    // definition, and is not restarted.
    let mut running =
        Supervisor::new(world.config(), None, vec!["OWNER".to_string()], false).unwrap();

    let file = write(&world, "fixes.toml", FIXES);
    let said = factory(&world, &["goal", "enable", "toy", &file]);
    assert!(said.status.success(), "{said:?}");
    let out = stdout(&said);
    let hash = goal_definition_hash(FIXES);

    // Stored, and in force.
    let stored = world.path(&format!("state/goals/toy/fixes/{hash}.toml"));
    assert_eq!(std::fs::read_to_string(&stored).unwrap(), FIXES);
    assert!(
        out.contains(&format!(
            "`fixes` is in force for `toy`: stored as {}",
            stored.display()
        )),
        "{out}"
    );

    // The running worker was told, through the engine's own command.
    assert_eq!(
        std::fs::read_to_string(world.path("engine-calls")).unwrap(),
        format!(
            "enabled toy fixes --config {}\n",
            world.path("config").display()
        )
    );
    assert!(
        out.contains("the tiers of project `toy` are running, under `fixes`"),
        "{out}"
    );

    // A bead was filed in the factory's own project to commit the file to
    // the release: open work, carrying the text and its hash.
    assert!(
        out.contains("filed filed-1 in `factory` to commit it as config/goals/toy/fixes.toml"),
        "{out}"
    );
    let bead = Source::Files {
        dir: world.path("factory-beads"),
    }
    .read("filed-1")
    .unwrap();
    assert_eq!(
        bead.title,
        "Commit goal definition `fixes` of `toy` to config/goals/toy/fixes.toml"
    );
    assert_eq!(bead.status, "open");
    assert!(
        bead.description.contains(&format!("```toml\n{FIXES}```")),
        "{}",
        bead.description
    );
    assert!(bead.description.contains(&hash), "{}", bead.description);
    assert!(
        bead.description
            .contains("The release has no file for it yet."),
        "{}",
        bead.description
    );
    assert!(bead.acceptance_criteria.contains(&hash));
    assert!(
        files_under(&world.path("beads"))
            .iter()
            .all(|path| !path.contains("filed"))
    );

    // `factory goal list` shows it, as a stored copy, beside the release's.
    let listed = factory(&world, &["goal", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let list = stdout(&listed);
    assert!(
        list.contains(&format!(
            "definition toy/fixes  stored copy {}\n    {}",
            &hash[..12],
            stored.display()
        )),
        "{list}"
    );
    assert!(
        list.contains("definition toy/main (default)  release "),
        "{list}"
    );

    // The service that was running all along opens a goal under it.
    let id = running
        .open_directly("toy", "fixes: Fix the crash")
        .unwrap();
    let goal = running.goals().find(|goal| goal.id == id).unwrap();
    assert_eq!(goal.definition, "fixes");
    assert_eq!(goal.definition_hash.as_deref(), Some(hash.as_str()));
    drop(running);

    // Once the bead is done and a release carries the text, the release's
    // file is in force and the stored copy is dropped as matching.
    std::fs::write(world.path("config/goals/toy/fixes.toml"), FIXES).unwrap();
    let list = stdout(&factory(&world, &["goal", "list"]));
    assert!(
        list.contains(&format!(
            "definition toy/fixes  release {}\n    {}",
            &hash[..12],
            world.path("config/goals/toy/fixes.toml").display()
        )),
        "{list}"
    );
    assert!(!world.path("state/goals/toy/fixes/current").exists());

    // Enabling what the release already says files nothing more.
    let said = factory(&world, &["goal", "enable", "toy", &file]);
    assert!(said.status.success(), "{said:?}");
    assert!(
        stdout(&said).contains(
            "the release's config/goals/toy/fixes.toml already has this text: no bead was filed"
        ),
        "{said:?}"
    );
    assert_eq!(files_under(&world.path("factory-beads")).len(), 1);
}

#[test]
fn an_invalid_definition_is_refused_with_its_problems_and_nothing_is_stored() {
    let world = factory_world("cli-enable-refused", true);
    for (name, text, problem) in [
        (
            "no-flow.toml",
            "id = \"fixes\"\nflow = \"no-such-flow\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
            "no-such-flow",
        ),
        (
            "two-defaults.toml",
            "id = \"fixes\"\ndefault = true\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n",
            "exactly one goal definition of the project must still be marked `default = true`",
        ),
        (
            "model.toml",
            "id = \"fixes\"\n[director]\nmodel = \"claude-haiku-1\"\neffort = \"high\"\n",
            "claude-haiku-1",
        ),
        ("not-toml.toml", "id = \"fixes\n", "TOML parse error"),
    ] {
        let file = write(&world, name, text);
        let said = factory(&world, &["goal", "enable", "toy", &file]);
        assert_eq!(said.status.code(), Some(1), "{name}: {said:?}");
        let err = stderr(&said);
        assert!(
            err.contains(&format!(
                "{file}: the goal definition was not put in force for `toy`:\n  - "
            )),
            "{name}: {err}"
        );
        assert!(err.contains(problem), "{name}: {err}");
        assert!(err.contains("nothing was stored"), "{name}: {err}");
        assert_eq!(stdout(&said), "", "{name}");
    }
    // Nothing stored, nobody told, nothing filed.
    assert_eq!(
        files_under(&world.path("state/goals/toy")),
        Vec::<String>::new()
    );
    assert!(!world.path("engine-calls").exists());
    assert_eq!(
        files_under(&world.path("factory-beads")),
        Vec::<String>::new()
    );
    let list = stdout(&factory(&world, &["goal", "list"]));
    assert!(!list.contains("toy/fixes"), "{list}");

    // A file that is not there is refused before anything is read.
    let said = factory(
        &world,
        &["goal", "enable", "toy", "/nonexistent/fixes.toml"],
    );
    assert_eq!(said.status.code(), Some(1), "{said:?}");
    assert!(
        stderr(&said).contains("/nonexistent/fixes.toml"),
        "{said:?}"
    );
}

#[test]
fn a_definition_of_the_factorys_own_goals_is_the_owners_to_commit() {
    let world = factory_world("cli-enable-own", true);
    let text = "id = \"main\"\n[director]\nmodel = \"claude-opus-5-5\"\neffort = \"high\"\n";
    let file = write(&world, "main.toml", text);
    let said = factory(&world, &["goal", "enable", "factory", &file]);
    assert!(said.status.success(), "{said:?}");
    let out = stdout(&said);
    assert!(
        out.contains(
            "filed filed-1 in `factory`, deferred: it is yours to commit as config/goals/factory/main.toml"
        ),
        "{out}"
    );
    let bead = Source::Files {
        dir: world.path("factory-beads"),
    }
    .read("filed-1")
    .unwrap();
    assert_eq!(bead.status, "deferred");
    assert!(
        bead.description.contains("the owner commits it by hand"),
        "{}",
        bead.description
    );
}

#[test]
fn what_could_not_be_done_after_storing_is_said_and_fails_the_command() {
    // No engine: nothing to tell, and the command says why.
    let world = factory_world("cli-enable-no-engine", false);
    let file = write(&world, "fixes.toml", FIXES);
    let said = factory(&world, &["goal", "enable", "toy", &file]);
    assert!(said.status.success(), "{said:?}");
    assert!(
        stdout(&said).contains("the factory has no engine ([temporal] is not set)"),
        "{said:?}"
    );

    // An engine that cannot be reached, and no project of the factory's
    // own: the definition stays in force, and each failure is named.
    let world = factory_world("cli-enable-failing", true);
    std::fs::remove_file(world.path("config/projects/factory.toml")).unwrap();
    std::fs::write(world.path("engine-fails"), "").unwrap();
    let file = write(&world, "fixes.toml", FIXES);
    let said = factory(&world, &["goal", "enable", "toy", &file]);
    assert_eq!(said.status.code(), Some(1), "{said:?}");
    let err = stderr(&said);
    assert!(
        err.contains("`fixes` is in force for `toy`, but the running worker was not told:"),
        "{err}"
    );
    assert!(err.contains("no server"), "{err}");
    assert!(
        err.contains("no bead was filed: no project is the factory's own"),
        "{err}"
    );
    let hash = goal_definition_hash(FIXES);
    assert!(
        world
            .path(&format!("state/goals/toy/fixes/{hash}.toml"))
            .is_file()
    );
    let list = stdout(&factory(&world, &["goal", "list"]));
    assert!(list.contains("definition toy/fixes  stored copy"), "{list}");
}
