# Contributing to aswitch

Thanks for your interest in improving aswitch.

## Development

```bash
# Build the workspace
cargo build

# Run the binary from source
cargo run -p aswitch-cli -- ls

# Run unit and integration tests
cargo test --workspace

# Lints
cargo clippy --workspace --no-deps

# Optional macOS keychain integration checks
cargo test -p aswitch-core macos_keychain -- --ignored
```

## Project layout

- `crates/aswitch-core/` library crate: switching transactions, identity
  extraction, usage collection, credential stores
- `crates/aswitch-cli/` binary crate: CLI subcommands and the TUI
- `assets/bundled-plugins/` bundled official plugin manifests synced from the
  separate `aswitch-plugin` repository

## Plugin manifests

Official plugin manifests live in the `aswitch-plugin` repository. To validate
a manifest locally:

```bash
cargo run -p aswitch-cli -- plugins validate path/to/plugin.toml
```

## Pull requests

- Keep changes focused; one logical change per PR.
- Add tests when fixing bugs or adding features.
- Run `cargo test --workspace` and `cargo clippy --workspace --no-deps` before
  pushing.
- Avoid committing personal credentials, tokens, or absolute paths to your
  home directory.

## Code style

- Source files are ASCII except for terminal-rendering glyphs (e.g. spinner
  braille) and test fixtures matching real provider output.
- Comments explain *why*, not *what*; well-named identifiers describe the
  *what* on their own.
- New public API in `aswitch-core` should have a unit test or doctest.

## Reporting issues

Please include:

- aswitch version (`aswitch --version`)
- platform (`uname -a` on macOS/Linux)
- the command you ran and the full output
- the relevant section of `~/.aswitch/logs/aswitch.log` if applicable

Avoid pasting credentials or refresh tokens; redact them before submitting.
