//! Cutting a release: `cargo xtask release-notes`, `package` and `release`.
//!
//! A release is a tag `v<version>` on a commit whose workspace version is
//! `<version>` and which carries its notes at `docs/releases/v<version>.md`.
//! Pushing the tag starts `.github/workflows/release.yml`, which builds one
//! archive per platform with `cargo xtask package`, writes `checksums.txt`,
//! and makes the GitHub release: the notes file's Highlights first, then
//! GitHub's generated list of pull requests. See docs/releasing.md.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, io};

/// The programs a release carries. `factory serve` installs the engine
/// built beside it, so they always ship together.
pub const PROGRAMS: [&str; 4] = [
    "factory",
    "factory-engine",
    "factory-engine-worker",
    "factory-dash",
];

/// The packages that build [`PROGRAMS`].
const PACKAGES: [&str; 3] = ["factory-cli", "factory-engine", "factory-dash"];

/// The sections of a release's Highlights, in the order they are written.
pub const SECTIONS: [&str; 4] = ["Added", "Changed", ":boom: Breaking Changes", "Fixed"];

/// `1.2.3` or `1.2.3-rc.1`: what a tag names after its `v`.
pub fn valid_version(version: &str) -> bool {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let number = |part: &&str| {
        !part.is_empty()
            && part.chars().all(|c| c.is_ascii_digit())
            && (part.len() == 1 || !part.starts_with('0'))
    };
    numbers.len() == 3
        && numbers.iter().all(number)
        && pre.is_none_or(|pre| {
            !pre.is_empty()
                && pre
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
        })
}

/// Whether GitHub should mark the release as a pre-release: anything
/// before 1.0.0, and anything with a pre-release part.
pub fn is_prerelease(version: &str) -> bool {
    version.starts_with("0.") || version.contains('-')
}

/// The workspace version in the root `Cargo.toml`.
pub fn workspace_version(manifest: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(manifest).ok()?;
    value
        .get("workspace")?
        .get("package")?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// The name Temporal's releases give a platform's archive, without the
/// extension: `software_factory_0.1.0_darwin_arm64`.
pub fn archive_name(version: &str, target: &str) -> Option<String> {
    let os = if target.contains("-apple-darwin") {
        "darwin"
    } else if target.contains("-linux-") {
        "linux"
    } else {
        return None;
    };
    let arch = match target.split('-').next()? {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return None,
    };
    Some(format!("software_factory_{version}_{os}_{arch}"))
}

/// Which section of the Highlights a commit's subject belongs in, by the
/// verb it starts with. A guess for the draft: the person cutting the
/// release moves what it gets wrong.
pub fn section_of(subject: &str) -> &'static str {
    let verb = subject
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match verb.as_str() {
        "add" | "allow" | "let" | "give" | "support" | "show" | "introduce" | "build" | "run" => {
            SECTIONS[0]
        }
        "remove" | "drop" | "rename" => SECTIONS[2],
        "fix" | "stop" | "keep" | "resolve" | "repair" => SECTIONS[3],
        _ => SECTIONS[1],
    }
}

/// A draft of a release's notes from the subjects of its commits.
pub fn draft_notes(version: &str, since: Option<&str>, subjects: &[String]) -> String {
    let mut out = String::from("## Highlights\n\n");
    out.push_str(&format!(
        "<!-- Draft for v{version} from the commits {}. Rewrite each line for someone who uses the factory, merge related ones, drop what nobody would notice, and delete empty sections and this comment. -->\n",
        match since {
            Some(tag) => format!("since {tag}"),
            None => "of the whole history".to_string(),
        }
    ));
    for section in SECTIONS {
        let lines: Vec<&String> = subjects
            .iter()
            .filter(|subject| section_of(subject) == section)
            .collect();
        out.push_str(&format!("\n### {section}\n\n"));
        for subject in lines {
            out.push_str(&format!("- {subject}\n"));
        }
    }
    out
}

/// What is wrong with a release's notes file, if anything.
pub fn notes_problems(notes: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !notes.starts_with("## Highlights\n") {
        problems.push("does not start with `## Highlights`".to_string());
    }
    if notes.contains("<!--") {
        problems.push("still has the draft's comment: finish the notes and remove it".to_string());
    }
    let mut seen = 0;
    for line in notes.lines().filter(|line| line.starts_with("### ")) {
        let name = &line[4..];
        match SECTIONS.iter().position(|section| *section == name) {
            Some(at) if at >= seen => seen = at + 1,
            Some(_) => problems.push(format!(
                "section `{name}` is out of order: they go {}",
                SECTIONS.join(", ")
            )),
            None => problems.push(format!(
                "`### {name}` is not a section of the Highlights: they are {}",
                SECTIONS.join(", ")
            )),
        }
    }
    let mut lines = notes.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(name) = line.strip_prefix("### ") {
            let mut empty = true;
            while let Some(next) = lines.peek() {
                if next.starts_with("### ") || next.starts_with("## ") {
                    break;
                }
                if !next.trim().is_empty() {
                    empty = false;
                }
                lines.next();
            }
            if empty {
                problems.push(format!("section `{name}` is empty: remove it"));
            }
        }
    }
    problems
}

fn notes_path(root: &Path, version: &str) -> PathBuf {
    root.join("docs/releases").join(format!("v{version}.md"))
}

fn git(root: &Path, args: &[&str]) -> Result<(bool, String), Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            command: format!("git {}", args.join(" ")),
            source,
        })?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn run_ok(command: &mut Command) -> Result<bool, Error> {
    let shown = format!("{command:?}");
    command
        .status()
        .map(|status| status.success())
        .map_err(|source| Error::Spawn {
            command: shown,
            source,
        })
}

fn last_tag(root: &Path) -> Result<Option<String>, Error> {
    let (ok, tag) = git(root, &["describe", "--tags", "--abbrev=0", "--match", "v*"])?;
    Ok(ok.then_some(tag).filter(|tag| !tag.is_empty()))
}

fn version_arg(args: &[String]) -> Result<(String, &[String]), Error> {
    let (version, rest) = args
        .split_first()
        .ok_or_else(|| Error::Usage("give the version, such as 0.1.0".to_string()))?;
    let version = version.strip_prefix('v').unwrap_or(version);
    if !valid_version(version) {
        return Err(Error::Usage(format!(
            "`{version}` is not a version such as 0.1.0 or 0.2.0-rc.1"
        )));
    }
    Ok((version.to_string(), rest))
}

/// `cargo xtask release-notes <version>`: write the draft notes.
pub fn notes_command(root: &Path, args: &[String]) -> Result<u8, Error> {
    let (version, rest) = version_arg(args)?;
    if !rest.is_empty() {
        return Err(Error::Usage(format!("unexpected `{}`", rest.join(" "))));
    }
    let path = notes_path(root, &version);
    if path.exists() {
        eprintln!("release-notes: {} exists; not overwritten", path.display());
        return Ok(1);
    }
    let since = last_tag(root)?;
    let range = match &since {
        Some(tag) => format!("{tag}..HEAD"),
        None => "HEAD".to_string(),
    };
    let (ok, log) = git(root, &["log", "--no-merges", "--format=%s", &range])?;
    if !ok {
        eprintln!("release-notes: `git log {range}` failed");
        return Ok(1);
    }
    let subjects: Vec<String> = log.lines().map(str::to_string).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    std::fs::write(&path, draft_notes(&version, since.as_deref(), &subjects)).map_err(io(&path))?;
    println!(
        "release-notes: drafted {} from {} commit(s); edit it before `cargo xtask release {version}`",
        path.display(),
        subjects.len()
    );
    Ok(0)
}

/// `cargo xtask package --target <triple> [--zig]`: build the programs for
/// one platform and put them in `target/dist/<archive>.tar.gz`. With
/// `--zig` the build goes through `cargo zigbuild`, for Linux targets
/// built on another host.
pub fn package_command(root: &Path, args: &[String]) -> Result<u8, Error> {
    let mut target = None;
    let mut zig = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--target" => target = args.next().cloned(),
            "--zig" => zig = true,
            other => return Err(Error::Usage(format!("unexpected `{other}`"))),
        }
    }
    let target = target.ok_or_else(|| Error::Usage("give the target with --target".to_string()))?;
    let manifest_path = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).map_err(io(&manifest_path))?;
    let version = workspace_version(&manifest).ok_or_else(|| Error::Manifest {
        path: manifest_path.clone(),
        reason: "no [workspace.package] version".to_string(),
    })?;
    let name = archive_name(&version, &target).ok_or_else(|| {
        Error::Usage(format!(
            "`{target}` is not a target a release is built for: darwin or linux, on aarch64 or x86_64"
        ))
    })?;
    // A Linux build through zig names the oldest glibc it runs on.
    let build_target = if zig && target.ends_with("-linux-gnu") {
        format!("{target}.2.28")
    } else {
        target.clone()
    };
    let mut build = Command::new("cargo");
    build
        .current_dir(root)
        .arg(if zig { "zigbuild" } else { "build" })
        .args(["--release", "--locked", "--target", &build_target]);
    for package in PACKAGES {
        build.args(["--package", package]);
    }
    if !run_ok(&mut build)? {
        eprintln!("package: the build for {target} failed");
        return Ok(1);
    }
    let built = root.join("target").join(&target).join("release");
    let dist = root.join("target/dist");
    let staged = dist.join(&name);
    if staged.exists() {
        std::fs::remove_dir_all(&staged).map_err(io(&staged))?;
    }
    std::fs::create_dir_all(&staged).map_err(io(&staged))?;
    for program in PROGRAMS {
        let from = built.join(program);
        std::fs::copy(&from, staged.join(program)).map_err(io(&from))?;
    }
    let archive = dist.join(format!("{name}.tar.gz"));
    // As in Temporal's archives, the programs are at the top of the
    // archive, with no directory around them.
    let mut tar = Command::new("tar");
    tar.current_dir(&staged)
        .arg("-czf")
        .arg(&archive)
        .args(PROGRAMS);
    if !run_ok(&mut tar)? {
        eprintln!("package: tar failed for {}", archive.display());
        return Ok(1);
    }
    std::fs::remove_dir_all(&staged).map_err(io(&staged))?;
    println!("{}", archive.display());
    Ok(0)
}

/// `cargo xtask release <version> [--dry-run]`: check that this commit can
/// be released as `<version>`, then tag it and push the tag.
pub fn release_command(root: &Path, args: &[String]) -> Result<u8, Error> {
    let (version, rest) = version_arg(args)?;
    let dry_run = match rest {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => return Err(Error::Usage(format!("unexpected `{}`", rest.join(" ")))),
    };
    let tag = format!("v{version}");
    let mut problems = Vec::new();

    let manifest_path = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).map_err(io(&manifest_path))?;
    match workspace_version(&manifest) {
        Some(found) if found == version => {}
        Some(found) => problems.push(format!(
            "Cargo.toml's workspace version is {found}: set it to {version} in the release commit"
        )),
        None => problems.push("Cargo.toml has no [workspace.package] version".to_string()),
    }
    let notes = notes_path(root, &version);
    match std::fs::read_to_string(&notes) {
        Ok(text) => problems.extend(
            notes_problems(&text)
                .into_iter()
                .map(|problem| format!("{}: {problem}", notes.display())),
        ),
        Err(_) => problems.push(format!(
            "{} is missing: draft it with `cargo xtask release-notes {version}`",
            notes.display()
        )),
    }
    let (_, dirty) = git(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        problems.push("the working tree has uncommitted changes".to_string());
    }
    let (_, local) = git(root, &["tag", "--list", &tag])?;
    if !local.is_empty() {
        problems.push(format!("the tag {tag} exists already"));
    }
    let (fetched, remote) = git(root, &["ls-remote", "--tags", "origin", &tag])?;
    if !fetched {
        problems.push("origin could not be asked for its tags".to_string());
    } else if !remote.is_empty() {
        problems.push(format!("origin has the tag {tag} already"));
    }
    let (_, head) = git(root, &["rev-parse", "HEAD"])?;
    let (_, pushed) = git(root, &["branch", "--remotes", "--contains", &head])?;
    if pushed.is_empty() {
        problems.push(format!(
            "HEAD ({}) is on no branch of origin: push the release commit first",
            &head[..head.len().min(10)]
        ));
    }
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("release: {problem}");
        }
        return Ok(1);
    }

    let report = crate::check::run(&crate::check::Options {
        root: root.to_path_buf(),
        offline: false,
        target_dir: None,
        quiet: false,
    })?;
    if !report.passed() {
        for stage in &report.failed {
            eprintln!("release: check {stage} FAILED");
        }
        return Ok(1);
    }
    if dry_run {
        println!(
            "release: {tag} can be cut from {}",
            &head[..head.len().min(10)]
        );
        return Ok(0);
    }
    let (tagged, _) = git(root, &["tag", "--annotate", &tag, "--message", &tag])?;
    if !tagged {
        eprintln!("release: could not make the tag {tag}");
        return Ok(1);
    }
    let (pushed, _) = git(root, &["push", "origin", &tag])?;
    if !pushed {
        eprintln!(
            "release: could not push {tag}; it is tagged here: `git push origin {tag}` to retry, or `git tag -d {tag}` to undo"
        );
        return Ok(1);
    }
    println!(
        "release: pushed {tag}. The release workflow builds the archives and makes the {}release on GitHub.",
        if is_prerelease(&version) { "pre-" } else { "" }
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_semver_with_an_optional_pre_release() {
        for good in ["0.1.0", "1.50.0", "0.2.0-rc.1", "10.0.0-alpha"] {
            assert!(valid_version(good), "{good}");
        }
        for bad in [
            "",
            "1.0",
            "v1.0.0",
            "01.0.0",
            "1.0.0-",
            "1.0.0-a..b",
            "1.x.0",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
        assert!(is_prerelease("0.1.0"));
        assert!(is_prerelease("1.0.0-rc.1"));
        assert!(!is_prerelease("1.0.0"));
    }

    #[test]
    fn archives_are_named_as_temporals_are() {
        assert_eq!(
            archive_name("0.1.0", "aarch64-apple-darwin").as_deref(),
            Some("software_factory_0.1.0_darwin_arm64")
        );
        assert_eq!(
            archive_name("0.1.0", "x86_64-unknown-linux-gnu").as_deref(),
            Some("software_factory_0.1.0_linux_amd64")
        );
        assert_eq!(archive_name("0.1.0", "x86_64-pc-windows-msvc"), None);
        assert_eq!(archive_name("0.1.0", "riscv64gc-unknown-linux-gnu"), None);
    }

    #[test]
    fn the_workspace_version_is_read_from_the_manifest() {
        let manifest = "[workspace]\nmembers = []\n[workspace.package]\nversion = \"0.3.1\"\n";
        assert_eq!(workspace_version(manifest).as_deref(), Some("0.3.1"));
        assert_eq!(workspace_version("[workspace]\n"), None);
    }

    #[test]
    fn a_draft_sorts_subjects_into_the_highlights_sections() {
        let subjects = [
            "Allow configuring the timeout",
            "Fix stale slot metrics",
            "Remove the old flag",
            "Run work side by side",
            "Make reviews shorter",
        ]
        .map(str::to_string);
        let draft = draft_notes("0.1.0", Some("v0.0.9"), &subjects);
        assert!(draft.starts_with("## Highlights\n"));
        assert!(draft.contains("from the commits since v0.0.9"));
        let at = |text: &str| draft.find(text).unwrap();
        assert!(at("### Added") < at("- Allow configuring"));
        assert!(at("- Run work side by side") < at("### Changed"));
        assert!(at("### Changed") < at("- Make reviews shorter"));
        assert!(at(":boom: Breaking Changes") < at("- Remove the old flag"));
        assert!(at("### Fixed") < at("- Fix stale slot metrics"));
        // A draft is not finished notes.
        assert!(!notes_problems(&draft).is_empty());
    }

    #[test]
    fn finished_notes_have_known_sections_in_order_and_none_empty() {
        let good = "## Highlights\n\n### Added\n\n- A thing.\n\n### Fixed\n\n- A bug.\n";
        assert!(
            notes_problems(good).is_empty(),
            "{:?}",
            notes_problems(good)
        );
        let problems =
            notes_problems("## Highlights\n\n### Fixed\n\n- A bug.\n\n### Added\n\n- A thing.\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("out of order"));
        let problems = notes_problems("## Highlights\n\n### Added\n\n### Notes\n\n- x\n");
        assert!(
            problems.iter().any(|p| p.contains("`Added` is empty")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("`### Notes`")),
            "{problems:?}"
        );
        assert!(!notes_problems("# Release\n").is_empty());
    }
}
