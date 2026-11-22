# iCloud-MCP

iCloud-MCP is a local MCP server for Apple Notes, Reminders, Contacts, Calendar, Photos and iCloud Drive, built on the [Skykeep](https://github.com/YashwanthKamireddi/Skykeep) Rust engine. The command is `skykeep-mcp`. It uses newline-delimited JSON-RPC over stdio and has no network listener. Reads use a private local SQLite cache.

Notes and Reminders can be read and changed; their changes enter a durable outbox for sync with iCloud. Contacts and Calendar are read-only. Photos can be read, and the `photos_write` tool changes photo details and albums and uploads files. iCloud Drive can be read and changed. File contents for Drive and Photos move only through one local exchange folder. Mail is not implemented and not planned, because upstream Skykeep has no Mail support.

This fork contains the MCP server and its four engine crates, not the upstream desktop or mobile applications. The source is at [hosakb/iCloud-MCP](https://github.com/hosakb/iCloud-MCP).

## Build

Install the Rust version pinned in [rust-toolchain.toml](rust-toolchain.toml). On Debian or Ubuntu, install a C build toolchain, `pkg-config` and `libdbus-1-dev`.

```sh
cargo build --locked --release -p skykeep-mcp
```

The binary is `target/release/skykeep-mcp`. The default workspace member is the server, so plain `cargo build` and `cargo test` select it. Only Linux is supported. The sign-in is kept in a Secret Service keyring or in a passphrase-protected file.

## Set up and run

From a terminal you open yourself, never an agent-owned terminal, run the guided setup:

```sh
target/release/skykeep-mcp setup
```

It asks for the state folder, where to keep the sign-in, your Apple Account sign-in and an approval mode, then prints what to paste into your MCP client's settings. `skykeep-mcp status` checks the setup without contacting Apple. If setup chose the passphrase-protected file, run `skykeep-mcp unlock` once after each restart. The Apple password is never stored.

## Tools and approval

The [MCP reference](crates/skykeep-mcp/README.md#tools) lists every tool, its fields and its errors. A permissions file in the state folder sets each tool or action to `allow`, `ask: client`, `ask: terminal` or `deny`. `ask: client` shows an approval card in the MCP client, `ask: terminal` waits for `skykeep-mcp approve` in your own terminal. Drive deletions always need a person, in every approval mode.

`photos_write` changes the personal Photos library one action at a time. It can upload a file from the exchange folder, set the favorite mark, title and caption, rating, keywords or date of a photo, create or rename an album, or move a photo to Recently Deleted. A move to Recently Deleted always needs a person, and its card never offers "Always allow". Every other action follows the approval mode and the permissions file, under names such as `photos_write.set_rating`. A photo or album that changed at Apple since the last `photos_refresh` is refused, not written over. A reply says what Apple's answer showed; nothing is read back. The tool can't delete a photo for good, edit pictures or change shared albums.

Approval protects against clients limited to MCP tools, not agents with shell access as the owner of the state directory. Such an agent can edit local state or invoke terminal helpers.

A successful Notes or Reminders write means queued locally, not verified in iCloud. See the [MCP reference](crates/skykeep-mcp/README.md) for sign-in, approval, the permissions file and limitations.

## Development

```sh
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Tests use synthetic fixtures and local mock servers. They do not require an Apple Account. See [contributing](.github/CONTRIBUTING.md), [architecture](ARCHITECTURE.md), [privacy](PRIVACY.md) and [security](SECURITY.md). The workflow builds and tests the MCP workspace; it does not publish releases or store packages.

## License and upstream attribution

This fork derives from Skykeep by Yashwanth Kamireddi. The retained engine crates keep their upstream names and code. Skykeep is free and open source under the [GPL-3.0-only license](LICENSE). iCloud-MCP is a modified fork of Skykeep, its changes are recorded in the git history, and it stays under GPL-3.0-only.

Copyright (C) 2026 Yashwanth Kamireddi. The inherited [funding configuration](.github/FUNDING.yml) supports the upstream project and remains unchanged.

This is an independent project, not affiliated with Apple Inc. Apple, iCloud and iPhone are trademarks of Apple Inc.