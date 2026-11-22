# Contributing

This fork develops the local Notes and Reminders MCP server, not the upstream desktop or mobile apps. Read the [architecture](../ARCHITECTURE.md) and [README](../README.md) before changing behavior.

Contributions use the [GPL-3.0-only license](../LICENSE). The [code of conduct](CODE_OF_CONDUCT.md) applies.

## Setup

Use the Rust version in `rust-toolchain.toml`, including rustfmt and clippy. Linux builds need a C compiler, `pkg-config` and D-Bus development headers. On Debian or Ubuntu:

```sh
sudo apt-get install build-essential pkg-config libdbus-1-dev
cargo build --locked -p skykeep-mcp
```

A Secret Service keyring is needed for real login, not for the normal synthetic test suite. Never enter credentials in an agent-owned terminal. Do not mount your home directory or personal keyring into test containers.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p skykeep-mcp
```

Plain `cargo test --locked` selects the default MCP member. `--workspace` also runs the four engine crates' tests. Add `--offline` when dependencies are cached. With a prepared container, disable its network and mount source read-only, with separate writable target and temporary directories. Loopback mock servers remain available inside that container.

The normal suite uses synthetic fixtures, throwaway libraries and mock HTTP services. Ignored real-keyring tests require an isolated Secret Service and are not part of the default gate. Live Apple tests and writes require separate authorization and a test account. Never use personal data in fixtures, logs or reports.

CI runs the format, workspace clippy, workspace test and MCP build commands on Ubuntu. Actions use pinned commit SHAs and the Rust version comes from the toolchain file. The workflow has read-only repository permissions and does not release or publish packages. Local checks do not establish that a GitHub Actions run has passed.

## Changes and review

- Keep patches focused. For a bug fix or new behavior, add a failing test before the implementation.
- Preserve account binding, private-state validation, session isolation and trusted-terminal approval. Approval is not a defense against an agent with shell access as the state owner.
- Keep secrets out of command arguments, environment variables, protocol traffic and logs.
- Use the existing library and durable outbox paths for writes. Do not claim remote verification when only queueing or delivery succeeded.
- Do not remove engine modules just because MCP does not expose their services. Retained engine types and tests can still depend on them.
- Review dependency updates separately. Workspace cleanup must preserve retained lockfile versions and checksums, not regenerate against newer cached versions.
- Stage explicit paths. Never stage local scratch directories, state, caches or private notes.

Describe what changed, why, the exact commands run and anything not verified. Link the issue. Preserve the GPL license and attribution to [Skykeep](https://github.com/YashwanthKamireddi/Skykeep) by Yashwanth Kamireddi.

The engine crates and their inherited READMEs remain unchanged by the app removal. Some historical app references in those files no longer describe this fork's setup. Use this guide for current commands. The identifier-only `src-tauri/tauri.conf.json` is retained because unchanged `quince-auth` embeds it; it is not an app target.