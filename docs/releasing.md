# Releasing

A release is a tag `v<version>` on a commit of the default branch, published
on GitHub with an archive per platform. Releases follow the shape of
Temporal's own: a `## Highlights` section written by hand, with `### Added`,
`### Changed`, `### :boom: Breaking Changes` and `### Fixed` in that order,
then GitHub's generated list of pull requests, new contributors and the full
changelog link; and archives named `software_factory_<version>_<os>_<arch>.tar.gz`
with a `checksums.txt` of their SHA-256 sums.

Every release before 1.0.0, and any with a pre-release part such as
`0.2.0-rc.1`, is marked as a pre-release on GitHub.

## Cutting one

1. Draft the notes from the commits since the last tag:

   ```bash
   cargo xtask release-notes 0.2.0   # writes docs/releases/v0.2.0.md
   ```

   The draft sorts commit subjects into the four sections by their first
   word. Rewrite it for someone who runs the factory: merge related lines,
   name the options, commands and files that changed, drop what nobody would
   notice, put anything that needs a reader to change their config or records
   under Breaking Changes, and remove empty sections and the draft's comment.

2. Set `version` under `[workspace.package]` in `Cargo.toml` to the new
   version, and commit it with the notes as "Prepare release v0.2.0". Land it
   on the default branch.

3. From that commit, check and tag:

   ```bash
   cargo xtask release 0.2.0 --dry-run   # everything but the tag
   cargo xtask release 0.2.0
   ```

   It refuses when the workspace version is not the one given, the notes are
   missing, unfinished or out of order, the working tree has changes, the tag
   exists here or on origin, or the commit is on no branch of origin. Then it
   runs `cargo xtask check`, tags the commit and pushes the tag.

4. The pushed tag starts `.github/workflows/release.yml`. It builds every
   platform on one macOS host with `cargo xtask package`, macOS natively and
   Linux through `cargo zigbuild` for glibc 2.28 and later, writes
   `checksums.txt`, and makes the release.

## Building an archive by hand

```bash
cargo xtask package --target aarch64-apple-darwin
cargo xtask package --target x86_64-unknown-linux-gnu --zig   # needs zig and cargo-zigbuild
```

Each archive is written to `target/dist/` and holds `factory`,
`factory-engine`, `factory-engine-worker` and `factory-dash` at its top level.
`factory serve` installs the engine that sits beside it, so the programs of one
archive are always used together.

## If a release goes wrong

A tag that has not been pushed is removed with `git tag -d v<version>`. Once
pushed, do not move it: fix what is wrong and release the next patch version.
A release workflow that failed before making the release can be run again from
the Actions tab on the same tag.
