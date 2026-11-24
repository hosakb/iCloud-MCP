# Architecture

The workspace has one executable, `skykeep-mcp` (the iCloud-MCP server), and four engine crates from upstream Skykeep. The server speaks newline-delimited JSON-RPC over stdio. Stdout carries protocol messages only; there is no HTTP listener. JSON-RPC batches are not supported.

## Crates

| Crate | Responsibility | Local dependencies |
|---|---|---|
| `skykeep-mcp` | MCP tools, private state, account binding, terminal helpers and Notes/Reminders scheduling | all four engine crates |
| `quince-auth` | SRP sign-in, two-factor routes, HTTP clients, sessions and keyring access | none |
| `quince-core` | SQLite library, schema, search and durable outbox | none |
| `quince-icloud` | Apple service requests, parsing and encoding | `quince-core` |
| `quince-sync` | Library locking, sync passes and queued change delivery | `quince-core`, `quince-icloud` |

The engine retains modules for other iCloud services because engine types and tests still depend on them. Their presence does not expose those services through MCP. The server's sync code selects only Notes and Reminders.

## Request and sync flow

The server validates tool arguments and opens its private state directory under an exclusive lock. Reads query the local library and report `source: local_cache`. Writes validate the operation and queue it in the library's durable outbox. Existing note edits require a `base_etag`; locked or read-only notes cannot be edited.

The scheduler refreshes every five minutes. A failed pass waits until the next interval. Explicit `sync` also refreshes before draining the queue, and sends nothing if the refresh fails. A sent operation is not a field-by-field remote verification, so responses retain `remote_verified: false`.

Several processes can share a state directory but operations take turns on its lock. A server can answer `initialize` while another process holds the lock; tool requests return `state_busy_retry` until it can acquire the lock. A new login session is picked up on a later request without restarting the server.

## Authentication and approval

Login, logout, approval and policy changes are terminal helpers, not MCP tools. Login accepts secrets only from a controlling terminal with echo disabled. The server stores session and trust tokens in the Linux Secret Service keyring and never stores the account password. A one-way account tag binds a library to one account. Cookies are sealed using a key held in the keyring.

The server does not import a desktop session. It uses the `skykeep-mcp` keyring service, with an entry per state directory. The file `src-tauri/tauri.conf.json` remains only as an identifier record: unchanged `quince-auth` source embeds it at compile time for its inherited default service. Its identifier is unchanged; it contains no desktop build or packaging configuration.

Destructive writes require a terminal approval tied to the requested operation. Optional require-all policy extends that to routine writes. This boundary assumes the client can use MCP tools only. An agent with shell access as the state owner can bypass it. See the [README](README.md#write-approval) and [security policy](SECURITY.md).

## Upstream maintenance

The five retained crates are unchanged by the app removal, including their inherited READMEs and historical comments. Some comments still name former desktop callers or scripts; those are not build dependencies. The identifier file above is the one retained compile-time dependency outside the crates.

Future upstream merges may conflict with deleted app files. Keep those deletions unless a reviewed engine change needs a file again. Do not remove engine modules merely because MCP does not expose them. Review dependency and behavior changes separately from repository cleanup.