# aswitch

`aswitch` is a Rust workspace for atomic account switching across AI agent
CLIs.

## Components

- `aswitch-core`: switching, identity extraction, login capture, usage collection, cache
- `aswitch-cli`: the end-user CLI and TUI
- `assets/bundled-plugins`: bundled official plugin manifests released into `~/.aswitch/plugins/`

## Status

- Supported providers: Claude Code, Codex, Gemini, opencode
- Supported platforms: macOS and Linux
- Early-stage project: command names and manifest details may still change

## Primary CLI Surface

- `aswitch save`
- `aswitch use`
- `aswitch ls`
- `aswitch login`
- `aswitch tui`
- `aswitch plugin ...`
- `aswitch rm`
- `aswitch init`

## Build

```bash
cargo build --release -p aswitch-cli
```

The compiled binary is:

```text
target/release/aswitch
```

## Usage

- See [USAGE.md](./USAGE.md)

## Related Repository

Official plugin manifests live in the companion `aswitch-plugin` repository.

## License

MIT
