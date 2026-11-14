# skykeep-mcp

A local MCP server for Apple Notes and Reminders, built on the Skykeep engine. It speaks newline-delimited JSON-RPC over stdio, keeps its own private library directory and has no network listener. No other iCloud service is synced or contacted.

## Build

```sh
cargo build --release -p skykeep-mcp
```

The workspace's default member is this crate, so `cargo build` and `cargo test` without `-p` also build only the headless server. The desktop and mobile apps stay in the workspace and build with their own `-p` flags. Linux needs `pkg-config` and the D-Bus development headers for the Secret Service client.

## Sign in

Sign-in happens in a terminal you open yourself, never through the MCP client or an agent:

```sh
skykeep-mcp login --state-dir ~/.local/share/skykeep-mcp
```

The command asks for your Apple Account, password and verification code with echo turned off. It reads only from the controlling terminal, never from a pipe, an argument or an environment variable. It refuses to start when it sees markers that an agent runtime launched it (`HERMES_AGENT`, `AI_AGENT`, `HERMES_SESSION_ID`, `CODEX_SANDBOX`, `CLAUDECODE`), because an agent-owned terminal can copy what you type into a transcript. That check only catches runtimes that set those markers; an agent with shell access can unset them (see "Write approval" below).

For the code you choose a route: `0` sends it to your trusted devices, or enter one of the listed phone IDs to get a text message. A wrong or expired code can be retried twice. Ctrl-C, Ctrl-D, Escape or three minutes without input cancel the login and nothing is saved.

What is kept afterwards:

- The session and trust tokens, in the desktop Secret Service keyring under the service name `skykeep-mcp`, one entry per state directory. They never go into a plain file, and the server never imports a session from the Skykeep desktop app.
- The account password is never stored. When the trust token expires, run `login` again.
- A one-way account tag in the library, so a directory can't be reused for another Apple Account. Use a new directory for a different account.

The state directory must be owned by you, mode `0700`, and contain no symlinks or files readable by others. The server and the helper commands take an exclusive lock on it for each operation. If `login` meets a request in progress it stops with `state_busy_retry`; run it again. A running server picks up the new session on its next request, without a restart.

To sign out:

```sh
skykeep-mcp logout --state-dir ~/.local/share/skykeep-mcp
```

This erases the keyring entry and the cookie file, which is sealed with a key held in the same keyring. Cached notes and reminders stay in the directory until you delete it. If the keyring is locked, the sign-out is recorded and finished the next time the keyring answers; until then no session is loaded.

## Run the server

```sh
skykeep-mcp --state-dir ~/.local/share/skykeep-mcp
```

Configure your MCP client to start that command. Stdout carries protocol messages only. JSON-RPC batches are not supported and get an `Invalid Request` error. Several clients can share one directory: operations take turns on the lock, and a server that starts while another holds it still answers `initialize`, then returns `state_busy_retry` from each tool call until the lock is free. The server never opens a keyring unlock dialog; if the keyring is locked or the session has expired, tools return `login_required` with instructions instead. While running it refreshes Notes and Reminders every five minutes; a failed pass waits for the next interval rather than retrying.

## Tools

| Tool | What it does |
|---|---|
| `auth_status` | Whether the directory is bound to an account, and how to sign in. Takes no arguments and never accepts secrets. |
| `sync` | Refreshes Notes and Reminders, then sends queued changes. Nothing is sent when the refresh fails. |
| `sync_status` | Last refresh per service, whether the last pass failed, and counts of pending and refused changes. No network access. |
| `notes_list`, `notes_folders`, `notes_search`, `notes_get` | Read the local cache. `notes_get` returns the plain text and the native formatting runs. |
| `notes_write` | Queues `create`, `save`, `move`, `pin` or `delete`. Edits to an existing note need the `base_etag` you read, and are refused for locked or read-only notes. |
| `reminders_lists`, `reminders_list`, `reminders_search`, `reminders_get` | Read the local cache. |
| `reminders_write` | Queues `create`, `edit`, `tick` (complete or reopen) or `delete`. An `edit` changes only the fields in `patch`; moving a reminder is an edit of `list_guid`. |

Reads come from the local cache and say so (`"source": "local_cache"`). A successful write means the change is queued in the library's durable outbox; `remote_verified` is always `false`, because the server does not read each field back from iCloud after sending.

## Write approval

Deletions never run straight from MCP. A deletion here means `delete`, a `move` into Recently Deleted (that is how iCloud deletes a note), and a `save` that leaves the note body empty. Creating a note in Recently Deleted is refused. For any of these the tool returns `approval_required` with a request ID, and you approve in your own terminal:

```sh
skykeep-mcp approve --state-dir ~/.local/share/skykeep-mcp
```

It asks for the request ID, shows the exact queued operation and queues it only after you type `approve`. To require this for every write, run `skykeep-mcp require-approval --state-dir DIR`; `allow-routine` switches back. Both commands ask you to type `confirm`.

There is no volume limit. Under the default policy an agent can queue any number of routine edits, ticks and saves that shorten a note without emptying it. If you want to see every write first, use `require-approval`.

What approval protects against: an agent that reaches this server only through MCP. It cannot approve its own requests, change the policy or sign in, because none of that is exposed as a tool. It does not protect against an agent that can run shell commands as your user. Such an agent can clear the marker variables and run `approve` or `allow-routine` in a pseudo-terminal, or edit the library file directly. The agent-marker check in `login` and the helper commands is a guard against accidents, not a security boundary. Don't give an agent shell access to the account that owns the state directory if you rely on approval.

## Limitations

- Notes with structures the engine can't round-trip are refused rather than flattened. Markdown export is not a lossless copy of a note.
- A write that iCloud accepts is reported as sent, not as verified field by field. Partial reminder writes are a known inherited gap.
- Attachments, folder and list management, and bulk operations are not exposed.
- Only Linux with a Secret Service keyring is supported for session storage. Without one, sign-in fails instead of falling back to a file.
