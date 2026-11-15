# zls-lsp

The Zig language server for Claude Code: definitions, references, hover and
diagnostics in `.zig` and `.zon` files.

It runs `zls` from the PATH, which must be installed and must match the
project's Zig release (zls 0.17.x for Zig 0.17.x):

```bash
brew install zls
```

Install the plugin for the user the factory runs as, so that headless runs
load it:

```bash
claude plugin marketplace add /path/to/software-factory/claude-plugins
claude plugin install zls-lsp@software-factory --scope user
```
