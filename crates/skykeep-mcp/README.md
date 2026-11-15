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

### Issue #7 staged implementation checkpoint

This candidate implements policy-controlled reads, Notes and Reminders mutations,
and bounded sync. Missing policy retains legacy behavior. It is not ready for
real-account rollout while the Hermes boolean-content and rendered-preview gates
below remain open. No mode has been activated on real state.

The human-only command is `skykeep-mcp policy --state-dir DIR MODE`, where MODE is
`reads_and_writes`, `writes_only`, or `yolo`. It requires a user-owned terminal,
literal `confirm`, persists a fresh revision bound to the account and directory
device/inode, and prints the saved record. It is never exposed as an MCP tool.
Account changes or copied policies fail closed until the human explicitly migrates
again. Existing terminal policy commands cannot override an activated new policy.
No migration back to legacy is provided. No real state was migrated in development.

| Mode | Content reads and refresh | Mutations, including deletion |
| --- | --- | --- |
| reads_and_writes | One-shot form approval | One-shot form approval |
| writes_only | No prompt | One-shot form approval |
| yolo | No prompt | No prompt |

2025-11-25 clients advertising `elicitation: {}` or `form: {}` receive standard
`elicitation/create` with a required boolean. Only `accept` with exactly
`content: {"approve": true}` authorizes execution. Unsupported versions, URL-only,
absent, malformed and unknown-only capabilities fail closed when consent is needed.
Legacy terminal proposals continue to work only in legacy state.

Consent is an in-memory continuation: the stdio loop resumes receiving messages
while the human decides, without retaining the directory lock. One consent can be
pending per connection; ping/status remain available. The originating RPC, complete
arguments, account, policy revision and directory identity are retained privately;
execution revalidates them under the operation lock. IDs use a UUID server namespace;
only response envelopes can complete consent. Decline, cancel, malformed replies,
policy changes, timeout (180 seconds), disconnect and replay cannot enqueue. No
durable consent record is written. EOF drops the pending continuation. Cancellation
after enqueue cannot roll back an already committed operation.

The complete create/read-request JSON is sent without truncation, with markup and
non-ASCII characters escaped as inert Unicode sequences. Existing-item mutations
show the cached item, body/runs or reminder details, affected subtasks, containers,
and prior queued operations. Execution checks that snapshot again under lock.
In `reads_and_writes`, a first consent authorizes reading this preview; a second
consent authorizes the mutation. A stale policy cannot authorize preview reads.
Frames are limited to 64 KiB before parsing, arguments to 32 KiB before cloning,
and escaped previews to 128 KiB before expansion. Parsed values also have depth
and node limits. Oversized operations are rejected, never silently truncated. The owner
must verify that the client exposes the entire preview before approving; source or
SDK tests do not establish UI rendering. Forms request only approval, never secrets.

Migration atomically marks existing pending writes refused with the reason
`mcp_policy_migration_quarantine` and invalidates their grants. It also blocks old
terminal proposals. Every new enqueue records the exact native payload and policy
revision in the same transaction. Coalescing requires previously authorized input
and replaces the resulting payload grant. Tries can change during retry; operation
bytes cannot. Changing policy quarantines even previously approved pending writes.

Before session validation, sync checks read permission and pending grants. In
`reads_and_writes`, startup/timer refresh is refused without issuing a form;
only an originating `sync` call can request one-shot refresh consent. The other
modes permit background refresh. Native sends recheck exact payload, account,
policy, directory and mutable cached inputs after refresh, before entering the
writer. Changed inputs fail closed. A sync has a 60-second async deadline and at
most 128 pending/refused records are admitted. Timeout after a send begins means
delivery may be uncertain; the original durable record remains for retry.
Partial native payload narrowing or changed cached inputs need fresh authorization
and are never silently blessed. These safeguards can refuse delivery after a
concurrent remote edit. Apple-side conflict/merge behavior remains native engine
behavior; local acceptance is not field-level remote verification.

Canonical read-only Hermes `tools/mcp_tool_sampling.py` currently returns empty
content on accept (`ElicitationHandler._result`). That cannot satisfy this server's
required boolean. Minimal owner-directed Hermes integration: collect the boolean
through the native form callback, return the actual validated content, and expose
the full inert preview on each supported surface. Until then Hermes acceptance
fails closed. No Hermes files were edited and no rendered UI was tested. OpenClaw
runtime interoperability is untested. The candidate is based on cleanup main;
deleted app files have not been restored. Independent security re-review is required.

References: [MCP elicitation](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation),
[MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools),
[Hermes configuration](https://hermes-agent.nousresearch.com/docs/reference/mcp-config-reference).
Client UI is client-owned. A same-user executable can forge consent or edit state;
MCP-only approvals cannot defend against that authority.

- Notes with structures the engine can't round-trip are refused rather than flattened. Markdown export is not a lossless copy of a note.
- A write that iCloud accepts is reported as sent, not as verified field by field. Partial reminder writes are a known inherited gap.
- Attachments, folder and list management, and bulk operations are not exposed.
- Only Linux with a Secret Service keyring is supported for session storage. Without one, sign-in fails instead of falling back to a file.