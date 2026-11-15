# skykeep-mcp

A local MCP server for Apple Notes and Reminders, built on the [Skykeep](https://github.com/YashwanthKamireddi/Skykeep) Rust engine. It uses newline-delimited JSON-RPC over stdio and has no network listener. Reads use a private local SQLite cache. Writes enter a durable outbox for sync with iCloud.

This fork contains the MCP server and its four engine crates, not the upstream desktop or mobile applications. Only Notes and Reminders are synced or contacted by the MCP server.

## Build

Install the Rust version pinned in [rust-toolchain.toml](rust-toolchain.toml). On Debian or Ubuntu, install a C build toolchain, `pkg-config` and `libdbus-1-dev`.

```sh
cargo build --locked --release -p skykeep-mcp
```

The binary is `target/release/skykeep-mcp`. The default workspace member is the server, so plain `cargo build` and `cargo test` select it. Session storage is supported only on Linux with a Secret Service keyring.

## Sign in and run

Sign in from a terminal you open yourself, never an agent-owned terminal:

```sh
target/release/skykeep-mcp login --state-dir ~/.local/share/skykeep-mcp
```

The command prompts for the account, password and verification code with echo off. Choose `0` for a trusted-device code or a listed phone ID for SMS. The password is not stored. Session and trust tokens stay in the system keyring; cookies are sealed with a key held there. Use a separate state directory for each Apple Account.

Configure your MCP client to launch the binary with these arguments:

```sh
target/release/skykeep-mcp --state-dir ~/.local/share/skykeep-mcp
```

The state directory must be owned by you, mode `0700`, without symlinks or shared files. A locked keyring or expired session produces `login_required`, not a keyring unlock dialog. The server refreshes Notes and Reminders every five minutes. `sync` requests a refresh and sends queued changes only after a successful refresh.

## Tools and write approval

The server exposes authentication and sync status, local Notes and Reminders reads and searches, and queued writes. The [MCP reference](crates/skykeep-mcp/README.md#tools) lists the tools, operation fields and error behavior.

Deletions, moves into Recently Deleted and saves that empty a note require approval in your own terminal:

```sh
target/release/skykeep-mcp approve --state-dir ~/.local/share/skykeep-mcp
```

To require approval for every write, run `require-approval --state-dir DIR`. `allow-routine --state-dir DIR` restores routine writes without approval. These commands ask for terminal confirmation. Default policy has no volume limit on routine edits.

Approval protects against clients limited to MCP tools, not agents with shell access as the owner of the state directory. Such an agent can edit local state or invoke terminal helpers. Agent-marker checks prevent accidents, not a hostile process running as your user.

A successful write means queued locally, not verified in iCloud. `remote_verified` is always `false`. Unsupported note structures are refused rather than flattened. Attachments, folder/list management and bulk operations are not exposed. See the [MCP reference](crates/skykeep-mcp/README.md) for full login, logout and approval behavior and limitations.

## Development

```sh
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Tests use synthetic fixtures and local mock servers. They do not require an Apple Account. See [contributing](.github/CONTRIBUTING.md), [architecture](ARCHITECTURE.md), [privacy](PRIVACY.md) and [security](SECURITY.md). The workflow builds and tests the MCP workspace; it does not publish releases or store packages.

## License and upstream attribution

This fork derives from Skykeep by Yashwanth Kamireddi. The retained engine crates keep their upstream names and code. Skykeep is free and open source under the [GPL-3.0-only license](LICENSE).

Copyright (C) 2026 Yashwanth Kamireddi. The inherited [funding configuration](.github/FUNDING.yml) supports the upstream project and remains unchanged.

This is an independent project, not affiliated with Apple Inc. Apple, iCloud and iPhone are trademarks of Apple Inc.