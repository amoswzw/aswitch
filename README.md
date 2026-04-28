# aswitch

> Atomic account switching for AI agent CLIs — Claude Code, Codex, Gemini, opencode.

[![CI](https://github.com/amoswzw/aswitch/actions/workflows/ci.yml/badge.svg)](https://github.com/amoswzw/aswitch/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-lightgrey)](#platforms)

`aswitch` lets you keep multiple accounts for the same AI agent (two Claude Pro
seats, a personal and a work Codex, several opencode provider sets) side by
side and flip between them in one command — atomically, with rollback on any
failure. It plugs into the agent's *own* credential store (macOS Keychain,
Linux libsecret, or a file like `~/.codex/auth.json`); the agent itself keeps
working as usual, you just point it at a different identity.

---

## At a glance

```text
$ aswitch ls
Saved accounts: 4
[1] claude-code/work
    Email: alice@example.com
    Quota: remaining=42%, reset=04-29 15:00
    Monthly Usage: req 9.0k | in 101.1k | out 8.3M
    Weekly Usage : req 1.7k | in  15.2k | out 2.0M
    Active       : yes

[2] codex/personal
    Email: alice@gmail.com
    Quota: remaining=14%, reset=04-29 18:35
    Monthly Usage: req 7.0k | in 684.5M | out 5.6M
    Active       : -

[3] gemini/default
    Email: alice@example.com
    Active       : -

[4] opencode/work-setup
    Providers: anthropic, openai, google
    Active       : -

$ aswitch use codex/personal
Switched codex → personal (shell scope). Restart the codex client to apply.
```

```text
$ aswitch ls --view status
registry version: 1
last switch: 2026-04-26 20:33

PLUGIN           STATUS   SOURCE   ACTIVE           COUNT    LAST_USED
claude-code      ok       user     work             2        2026-04-26 06:55
codex            ok       user     personal         2        2026-04-26 20:33
gemini           ok       user     default          1        2026-04-24 20:55
opencode         ok       user     work-setup       1        2026-04-25 09:10
```

---

## Why aswitch

| Pain | What aswitch does |
| --- | --- |
| Two Claude Pro seats, one machine | Saves each seat's Keychain entry as a named account; one command flips the live credential. |
| Codex login keeps clobbering the previous account | Backups are kept in `~/.aswitch/`, switch is a transaction, any failure rolls back. |
| Project A wants the work account, Project B the personal one | `--scope project` writes a `.aswitch.toml`, picked up automatically when you `cd` in. |
| "Did I burn through my quota this month?" | `aswitch ls` (default view) reads the agent's own JSONL logs and shows monthly / weekly usage per account. |
| New agent CLI launched yesterday | Drop a TOML manifest in `~/.aswitch/plugins/` — no recompile. |

What aswitch deliberately doesn't do: no daemon, no auto-switching, no
quota alerts, no proxying the agent's network traffic, no Windows. It is a
focused, on-demand switcher.

---

## Install

### Homebrew (recommended)

```bash
brew tap amoswzw/tap
brew install aswitch
```

### From source

```bash
cargo install --git https://github.com/amoswzw/aswitch aswitch-cli
```

### From a clone

```bash
git clone https://github.com/amoswzw/aswitch.git
cd aswitch
cargo build --release -p aswitch-cli
./target/release/aswitch --version
```

On first run `aswitch` creates `~/.aswitch/` and unpacks the four bundled plugin
manifests into `~/.aswitch/plugins/`.

---

## Quickstart

```bash
# 1. Capture an account (after you've logged in to the agent normally)
aswitch save claude-code/work

# 2. Or run the agent's native login and save in one step
aswitch login codex/personal

# 3. List what you have
aswitch ls

# 4. Enable shell integration once per shell (zsh/bash)
eval "$(aswitch init)"

# 5. Switch in this shell
aswitch use codex/personal

# 6. Or open the TUI and pick with arrow keys
aswitch
```

---

## Three scopes

`aswitch use` chooses where the switch is recorded. Precedence is
**shell > project > global**.

| Scope | What it changes | Affects |
| --- | --- | --- |
| `shell` *(default)* | An env-var stub injected by `aswitch init` | Just this shell |
| `project` | Writes `.aswitch.toml` in the current directory tree | Anyone working under that tree |
| `global` | The agent's own native credential location | Every shell, every project |

```bash
aswitch use codex/work                       # shell only
aswitch use codex/work --scope project       # this repo
aswitch use codex/work --scope global        # the whole machine
aswitch use --off                            # turn the shell scope off
aswitch use --off --scope project            # remove .aswitch.toml binding
```

---

## TUI

```bash
aswitch        # opens the TUI when stdin is a tty
aswitch tui    # explicit
```

| Key | Action |
| --- | --- |
| `j` / `k` or arrows | Move |
| `Enter` | Switch the selected account globally |
| `Tab` / `1` / `2` | Move between panels |
| `w` | Cycle the usage time window |
| `s` | Cycle the usage source (local logs / provider API / both) |
| `R` | Refresh usage data |
| `?` | Help |
| `q` | Quit |

---

## Supported providers

Bundled with the binary; updates ship in [`amoswzw/aswitch-plugin`](https://github.com/amoswzw/aswitch-plugin).

| Plugin | Credential store | Login command | Notes |
| --- | --- | --- | --- |
| `claude-code` | macOS Keychain *(file fallback on Linux)* | `claude login` | Backs up `~/.claude/.claude.json` alongside |
| `codex` | `~/.codex/auth.json` | `codex login` | Identity from the JWT in `tokens.id_token` |
| `gemini` | macOS Keychain / libsecret | `gemini` | Aux files: `~/.gemini/.env`, `settings.json` |
| `opencode` | `~/.local/share/opencode/auth.json` | `opencode auth login` | Treats the multi-provider `auth.json` as one snapshot |

Adding a new agent only takes a `plugin.toml` — see the
[manifest schema](https://github.com/amoswzw/aswitch-plugin/blob/main/docs/manifest-schema.md).

## Platforms

macOS 12+ (arm64 / x64) and Linux (glibc 2.31+, arm64 / x64). Windows is not
supported and not planned.

---

## How it works

Every switch runs as a transaction:

1. Take the file lock on `~/.aswitch/.lock`.
2. Read the currently active credential and any aux files; back them up.
3. Read the target account's saved snapshot.
4. Atomically write the snapshot to the live location (`security` for
   Keychain, `tmp + rename` for files).
5. Update `~/.aswitch/registry.json`.

If any step fails the rollback stack runs in reverse, restoring the live
location to the pre-switch state. The exit code distinguishes
**fully rolled back (10)** from **rollback partially failed (11)**, so
automation never has to guess the state.

```text
~/.aswitch/
├── plugins/                  # bundled and user manifests
├── accounts/<plugin>/<alias> # per-account credential + aux + cached identity
├── usage_cache/              # snapshots from `aswitch ls` (TTL'd)
├── registry.json             # active accounts and metadata
└── logs/aswitch.log          # rotating log
```

---

## Components

- **`aswitch-core`** — switching engine, identity extraction, login capture, usage collection, cache.
- **`aswitch-cli`** — the `aswitch` binary and TUI.
- **`assets/bundled-plugins/`** — TOML manifests baked into the binary.

The full feature reference lives in [USAGE.md](./USAGE.md).

## Related repositories

- [`amoswzw/aswitch-plugin`](https://github.com/amoswzw/aswitch-plugin) — official plugin manifests and authoring docs.
- [`amoswzw/homebrew-tap`](https://github.com/amoswzw/homebrew-tap) — Homebrew formula.

## License

MIT — see [LICENSE](./LICENSE).
