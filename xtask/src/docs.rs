//! `cargo xtask docs`: the glossary defines each term once, every link in the
//! docs leads to a file that exists, and AGENTS.md stays short.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{self, Error};

/// The terms every other document relies on.
pub const REQUIRED_TERMS: [&str; 16] = [
    "project", "bead", "unit", "flow", "step", "role", "outcome", "artifact", "run", "job",
    "result", "slot", "account", "session", "tick", "gate",
];

/// AGENTS.md may have at most this many lines outside its generated blocks.
pub const AGENTS_MAX_LINES: usize = 79;

const GENERATED_BEGIN: &str = "<!-- BEGIN BEADS";
const GENERATED_END: &str = "<!-- END BEADS";

/// Run every check and return the problems found. Empty means the docs are sound.
pub fn run(root: &Path) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();

    let glossary_path = root.join("docs/glossary.md");
    let glossary = fs::read_to_string(&glossary_path).map_err(error::io(&glossary_path))?;
    problems.extend(glossary_problems(&glossary));

    let mut files = vec![root.join("AGENTS.md"), root.join("CLAUDE.md")];
    let docs_dir = root.join("docs");
    for entry in fs::read_dir(&docs_dir).map_err(error::io(&docs_dir))? {
        let path = entry.map_err(error::io(&docs_dir))?.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
    files.sort();
    for file in &files {
        let text = fs::read_to_string(file).map_err(error::io(file))?;
        let shown = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        for target in missing_links(file, &text) {
            problems.push(format!("{shown}: link to missing file `{target}`"));
        }
    }

    let agents_path = root.join("AGENTS.md");
    let agents = fs::read_to_string(&agents_path).map_err(error::io(&agents_path))?;
    problems.extend(agents_problems(&agents));

    Ok(problems)
}

/// The terms a glossary defines: its level-two headings, lowercased.
fn defined_terms(glossary: &str) -> Vec<String> {
    prose_lines(glossary)
        .filter_map(|line| line.strip_prefix("## "))
        .map(|term| term.trim().to_lowercase())
        .collect()
}

pub fn glossary_problems(glossary: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for term in defined_terms(glossary) {
        if !seen.insert(term.clone()) {
            problems.push(format!("docs/glossary.md: `{term}` is defined twice"));
        }
    }
    for term in REQUIRED_TERMS {
        if !seen.contains(term) {
            problems.push(format!("docs/glossary.md: `{term}` is not defined"));
        }
    }
    problems
}

/// The lines of a Markdown document that are not inside a fenced code block.
fn prose_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut in_fence = false;
    text.lines().filter(move |line| {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            return false;
        }
        !in_fence
    })
}

/// The targets of the Markdown links in a document, outside code blocks.
fn link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in prose_lines(text) {
        let mut rest = line;
        while let Some(start) = rest.find("](") {
            rest = &rest[start + 2..];
            let Some(end) = rest.find(')') else { break };
            targets.push(rest[..end].trim().to_string());
            rest = &rest[end + 1..];
        }
    }
    targets
}

/// Links in `text` to local files that do not exist, resolved against the
/// directory of `file`. Links to other sites and to anchors are not checked.
pub fn missing_links(file: &Path, text: &str) -> Vec<String> {
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    link_targets(text)
        .into_iter()
        .filter(|target| {
            let external = target.contains("://") || target.starts_with("mailto:");
            let path = target.split('#').next().unwrap_or("");
            !external && !path.is_empty() && !resolve(&dir, path).exists()
        })
        .collect()
}

fn resolve(dir: &Path, target: &str) -> PathBuf {
    if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        dir.join(target)
    }
}

pub fn agents_problems(agents: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut in_generated = false;
    let mut own_lines = 0;
    for line in agents.lines() {
        if line.starts_with(GENERATED_BEGIN) {
            in_generated = true;
        } else if line.starts_with(GENERATED_END) {
            in_generated = false;
        } else if !in_generated {
            own_lines += 1;
        }
    }
    if in_generated {
        problems.push("AGENTS.md: a generated block is never closed".to_string());
    }
    if own_lines > AGENTS_MAX_LINES {
        problems.push(format!(
            "AGENTS.md: {own_lines} lines outside the generated blocks; the limit is {AGENTS_MAX_LINES}"
        ));
    }
    let targets = link_targets(agents);
    for doc in ["docs/architecture.md", "docs/glossary.md"] {
        if !targets.iter().any(|target| target == doc) {
            problems.push(format!("AGENTS.md: no link to {doc}"));
        }
    }
    if !agents.contains("cargo xtask check") {
        problems.push("AGENTS.md: the check command `cargo xtask check` is not listed".to_string());
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glossary(terms: &[&str]) -> String {
        terms
            .iter()
            .map(|term| format!("## {term}\n\nA definition.\n\n"))
            .collect()
    }

    #[test]
    fn a_complete_glossary_has_no_problems() {
        assert_eq!(
            glossary_problems(&glossary(&REQUIRED_TERMS)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_term_defined_twice_is_a_problem() {
        let mut terms = REQUIRED_TERMS.to_vec();
        terms.push("Unit");
        let problems = glossary_problems(&glossary(&terms));
        assert_eq!(problems, ["docs/glossary.md: `unit` is defined twice"]);
    }

    #[test]
    fn a_missing_term_is_a_problem() {
        let problems = glossary_problems(&glossary(&REQUIRED_TERMS[1..]));
        assert_eq!(problems, ["docs/glossary.md: `project` is not defined"]);
    }

    #[test]
    fn a_heading_inside_a_code_block_defines_nothing() {
        let text = format!("{}```\n## unit\n```\n", glossary(&REQUIRED_TERMS));
        assert_eq!(glossary_problems(&text), Vec::<String>::new());
    }

    #[test]
    fn a_link_to_a_missing_file_is_found() {
        let dir = std::env::temp_dir().join(format!("xtask-docs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("here.md"), "").unwrap();
        let text = "[ok](here.md) [anchor](here.md#part) [gone](gone.md) [web](https://example.com/x.md) [top](#top)\n```\n[code](nowhere.md)\n```\n";
        let missing = missing_links(&dir.join("page.md"), text);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(missing, ["gone.md"]);
    }

    #[test]
    fn agents_file_must_link_both_docs_and_list_the_check_command() {
        let good = "[a](docs/architecture.md) [g](docs/glossary.md)\n`cargo xtask check`\n";
        assert_eq!(agents_problems(good), Vec::<String>::new());
        assert_eq!(agents_problems("").len(), 3);
    }

    #[test]
    fn agents_file_length_ignores_generated_blocks() {
        let own = "line\n".repeat(AGENTS_MAX_LINES - 2);
        let generated = format!(
            "{GENERATED_BEGIN} -->\n{}{GENERATED_END} -->\n",
            "generated\n".repeat(500)
        );
        let links = "[a](docs/architecture.md) [g](docs/glossary.md)\n`cargo xtask check`\n";
        let ok = format!("{own}{generated}{links}");
        assert_eq!(agents_problems(&ok), Vec::<String>::new());
        let too_long = format!("{ok}one more\n");
        assert_eq!(agents_problems(&too_long).len(), 1);
    }
}
