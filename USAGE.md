# aswitch Usage Guide

## Overview

`aswitch` manages multiple saved account snapshots for supported AI agent CLIs.

The public command surface is:

```text
aswitch
aswitch tui
aswitch save
aswitch login
aswitch ls
aswitch use
aswitch rm
aswitch init
```

The canonical account identifier is:

```text
<plugin>/<alias>
```

Examples:

```text
codex/work
claude-code/personal
gemini/default
```

## Build

Build a release binary:

```bash
cargo build --release -p aswitch-cli
```

The binary is written to:

```text
target/release/aswitch
```

## First Run

On first run, `aswitch` creates `~/.aswitch/` and releases bundled plugin manifests into:

```text
~/.aswitch/plugins/
```

The advanced plugin maintenance command still exists:

```bash
aswitch plugin ls
```

## Default UI

Running `aswitch` without a subcommand opens the default TUI in an interactive terminal.

You can also open it explicitly:

```bash
aswitch tui
```

## Scope Model

`aswitch use` supports three scopes:

- `shell`: current shell only
- `project`: current directory tree through `.aswitch.toml`
- `global`: native client location

Precedence is:

```text
shell > project > global
```

That means a shell override wins over a project binding, and a project binding wins over the global live credentials.

## Shell Integration

Shell scope needs shell integration because a child process cannot directly modify the parent shell environment.

Enable it once per shell:

```bash
eval "$(aswitch init)"
```

Or explicitly:

```bash
eval "$(aswitch init zsh)"
eval "$(aswitch init bash)"
```

Without shell integration:

- `aswitch use <account>` fails for shell scope
- `aswitch use --off` fails for shell scope
- `aswitch use --scope project ...` still writes `.aswitch.toml`
- `aswitch use --scope global ...` still works because it writes the native client location directly

## Save Accounts

Save the currently active live credentials into a named account:

```bash
aswitch save codex/work
```

Split form:

```bash
aswitch save work --plugin codex
```

Overwrite an existing account:

```bash
aswitch save codex/work --force
```

JSON output:

```bash
aswitch save codex/work --json
```

## Native Login and Save

Run the plugin's native login flow and save the result:

```bash
aswitch login codex/work
```

Or:

```bash
aswitch login codex --as work
```

If you only pass the plugin, `aswitch` prompts for the alias after login:

```bash
aswitch login codex
```

## List Views

`ls` is the single read-oriented command.

Saved accounts:

```bash
aswitch ls
```

Effective current scope:

```bash
aswitch ls --view current
aswitch ls --view current --explain
```

Project binding:

```bash
aswitch ls --view project
```

Registry and plugin status:

```bash
aswitch ls --view status
```

Plugin filter:

```bash
aswitch ls --plugin codex
aswitch ls --view current --plugin codex
```

JSON output:

```bash
aswitch ls --json
aswitch ls --view current --json
```

## Switch Accounts

Shell scope is the default:

```bash
aswitch use codex/work
```

Project scope:

```bash
aswitch use codex/work --scope project
```

Global scope:

```bash
aswitch use codex/work --scope global
```

You can also select by:

- alias, if it is unique across plugins
- row number from `aswitch ls`
- alias with `--plugin`

Examples:

```bash
aswitch ls
aswitch use 2
aswitch use work
aswitch use work --plugin codex
```

Turn a scope off:

```bash
aswitch use --off
aswitch use --off --scope project
aswitch use --off --scope global --plugin codex
```

If you omit `--plugin`, global off clears all globally active tracked plugins.

## Remove Accounts

Remove a saved account:

```bash
aswitch rm codex/work
```

If removing the currently active managed account:

```bash
aswitch rm codex/work --force
```

## TUI

Inside the main TUI:

- `j/k` or arrow keys move
- `Enter` switches the selected account globally
- `w` changes the usage window
- `s` changes the usage source
- `R` refreshes usage
- `Tab` or `1/2` switches panels
- `?` opens help
- `q` quits

## Config Directory Override

For testing or isolated environments, use:

```bash
aswitch --config-dir /tmp/demo-config ls
```

This affects all commands.

## Recommended Workflow

```bash
# 1. log in natively and save the result
aswitch login codex/work

# 2. save another account
aswitch login codex/personal

# 3. inspect saved accounts
aswitch ls

# 4. enable shell integration
eval "$(aswitch init)"

# 5. switch accounts in the current shell
aswitch use codex/work

# 6. inspect usage in the TUI
aswitch
```

## Notes

- `save` captures the currently active live credentials.
- `use` defaults to shell scope.
- `use --scope project` stores folder-scoped bindings in `.aswitch.toml`.
- `login` does not replace the native login flow; it wraps it and saves the resulting credentials.
- `init` is required for shell scope.
- For automation, prefer explicit selectors such as `codex/work`.
