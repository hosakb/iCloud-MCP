# skykeep-mcp

A local MCP server for Apple Notes, Reminders, Contacts, Calendar, Photos and iCloud Drive, built on the Skykeep engine. It speaks newline-delimited JSON-RPC over stdio, keeps its own private library directory and has no network listener. Notes and Reminders can be changed; Contacts, Calendar, Photos and Drive are read-only, and Photos and Drive expose metadata only (no pictures, no file contents). Mail is not implemented.

## Architecture

![Skykeep MCP: Hermes talks to the MCP server over stdio, which syncs with Apple iCloud. Live today: Notes and Reminders (read and write), Contacts, Calendar, Photos and Drive (read-only). Planned with no route built yet: Mail, and repeating reminders with alerts](docs/architecture.webp)

Edit `docs/architecture.html` and re-export `docs/architecture.webp` whenever a route moves from planned to live.

## Build

```sh
cargo build --release -p skykeep-mcp
```

The workspace's default member is this crate, so `cargo build` and `cargo test` without `-p` also build only the headless server. The desktop and mobile apps stay in the workspace and build with their own `-p` flags. Linux needs `pkg-config` and the D-Bus development headers for the Secret Service client: on Debian or Ubuntu `sudo apt install pkg-config libdbus-1-dev`, on Fedora `sudo dnf install pkgconf-pkg-config dbus-devel`, on Arch `sudo pacman -S pkgconf dbus`. Rust itself comes from [rustup.rs](https://rustup.rs).

## Get started

After the build, the program is at `target/release/skykeep-mcp`. Install it where your terminal finds programs:

```sh
mkdir -p ~/.local/bin
install -m 755 target/release/skykeep-mcp ~/.local/bin/
```

If your terminal then says it can't find `skykeep-mcp`, type `~/.local/bin/skykeep-mcp` wherever this guide says `skykeep-mcp`. `skykeep-mcp --help` lists every command.

You need a terminal window that you open yourself. Don't run these commands through an AI assistant or chat: what you type there can end up in a transcript.

1. Run the guided setup:

   ```sh
   skykeep-mcp setup
   ```

   It walks you through five steps and tells you what each one does:

   1. **Folder.** Where Skykeep keeps its private data. Press Enter to use the suggested folder, `~/.local/share/skykeep-mcp`, or type another one as a full path starting with `/` or `~`. Setup asks before it creates a folder. With another folder, add `--state-dir FOLDER` to every command; setup and status print each command with it already filled in.
   2. **Where your sign-in is kept.** Your computer's password store when it has one, otherwise a protected file (see below).
   3. **Apple sign-in.** Your Apple Account, your password and then a verification code. Your account, password and code are not shown as you type. You pick a number for where Apple sends the code: your Apple devices, or a text message to one of your phone numbers if Apple lists any. At the code you can type `b` to go back and pick the other way without typing your password again.
   4. **Approval mode.** When the assistant has to ask you before it acts:

      | Choice | Reads and refreshes | Changes, including deletion |
      | --- | --- | --- |
      | 1. Ask before every read and every change (recommended, `reads_and_writes`) | Asks you in the MCP client | Asks you in the MCP client |
      | 2. Read without asking, ask before every change (`writes_only`) | No question | Asks you in the MCP client |
      | 3. Never ask, not even before deleting (`yolo`, you type `confirm`) | No question | No question |
      | 4. No approval mode, for clients without approval forms | No question | Routine changes: no question. Deletions wait until you run `skykeep-mcp approve` in a terminal |

      Choices 1 and 2 need an MCP client that can show Skykeep's approval forms. Hermes can't yet: with it, every request that needs approval fails with `approval_requires_form_client`. Use choice 4 for such a client. Choice 4 keeps a folder's earlier `require-approval` setting: then every change waits for `skykeep-mcp approve`, and setup and status say so. Once a folder has had choice 1, 2 or 3, it can't go back to 4; set up a new folder instead.
   5. **Check.** Setup checks everything it set up and prints what to paste into your MCP client's settings: a JSON line for most clients, and a `config.yaml` block for Hermes.

   You can run `setup` again at any time. It checks the saved sign-in with Apple, then asks whether to keep the setup, sign in again in the same folder (same Apple Account), switch to another Apple Account, or quit. If Apple no longer accepts the saved sign-in, setup says so and goes straight to signing in again. A folder belongs to one Apple Account, so switching sets up a new folder for the other account and leaves the first one as it is. If setup stopped part way, the next run continues with the first step that is not done.

2. Check that everything is ready:

   ```sh
   skykeep-mcp status
   ```

   It only looks at this computer: it never contacts Apple and never asks for a password. Each problem comes with the next step to take. It ends with an error code when something needs fixing, so scripts can check it.

3. Only if setup chose the protected file: after each restart of the computer, run this once before you use the assistant:

   ```sh
   skykeep-mcp unlock
   ```

### Where your sign-in is kept

Skykeep never writes your sign-in to a plain file. There are two options:

- **Your computer's password store** (the system keyring, such as GNOME Keyring or KWallet). Setup uses it when it is running and unlocked. Nothing more to do after a restart once you have logged in to your desktop.
- **A protected file** in the Skykeep folder, for computers without a usable password store, such as a server you reach over SSH. You choose a passphrase of at least 12 characters, typed twice. The file is encrypted with it (AES-256-GCM, key derived with PBKDF2-HMAC-SHA256). After each restart, `skykeep-mcp unlock` asks for the passphrase once and gives the server the key through the Linux kernel's own key storage, so the server can read the file without a desktop. The key is never written to a file. Like the system keyring, it can be read by programs running as your user, and by nobody else. Until you unlock, the server's tools answer `locked: run skykeep-mcp unlock in a terminal`. To make the key expire earlier than the next restart, use `skykeep-mcp unlock --hours 8`.

If neither is available, setup stops and says so instead of keeping the sign-in anywhere less safe. If you are inside Docker or a similar container, run Skykeep directly on the computer; on a desktop, log in to the desktop session first. When the option you chose stops working (the keyring is gone, or the kernel key storage is blocked), setup offers the other one. Switching asks you to type `switch`, removes the sign-in kept the old way, and you sign in to Apple again. If the old keyring can't be reached at all, setup says that an older entry may stay there unused; before the folder ever uses that keyring again, setup erases it. A locked keyring is never switched away from: unlock it instead. If you forget the passphrase, run `setup` again and choose to start over with a new one; you then sign in to Apple again, and your cached data stays.

Your Apple password is never stored. When Apple asks you to sign in again, run `setup` again and choose to sign in again in this folder.

### Sign in and out by hand

`setup` uses the same sign-in as these commands, which still work on their own:

```sh
skykeep-mcp login --state-dir ~/.local/share/skykeep-mcp
skykeep-mcp logout --state-dir ~/.local/share/skykeep-mcp
```

`login` asks for your Apple Account, password and verification code with echo turned off. It reads only from the terminal you opened, never from a pipe, an argument or an environment variable. It refuses to start when it sees markers that an agent runtime launched it (`HERMES_AGENT`, `AI_AGENT`, `HERMES_SESSION_ID`, `CODEX_SANDBOX`, `CLAUDECODE`), because an agent-owned terminal can copy what you type into a transcript. That check only catches runtimes that set those markers; an agent with shell access can unset them (see "Write approval" below).

For the code you pick a number from a short list: one sends the code to your Apple devices, and each phone number Apple lists (shown as Apple masks it) gets a text message or a call. At the code prompt, type `b` to go back and pick another way; one sign-in asks Apple for at most four codes. A wrong or expired code can be retried twice. Ctrl-C, Ctrl-D, Escape or three minutes without input cancel the sign-in and nothing is saved.

What is kept afterwards:

- The session and trust tokens, in the storage chosen above. With the system keyring they are under the service name `skykeep-mcp`, one entry per folder. The server never imports a session from the Skykeep desktop app.
- A one-way account tag in the library, so a folder can't be reused for another Apple Account. Use a new folder for a different account.

The folder must be owned by you, mode `0700`, and contain no symlinks or files readable by others. The server and the helper commands take an exclusive lock on it for each operation. If a command meets a request in progress it stops with `state_busy_retry`; run it again. A running server picks up the new session on its next request, without a restart.

`logout` erases the stored session and the cookie file, which is sealed with a key kept beside the session. Cached data stays in the folder until you delete it. If the system keyring is locked, the sign-out is recorded and finished the next time the keyring answers; until then no session is loaded. With the protected file, run `unlock` first.

## Run the server

```sh
skykeep-mcp --state-dir ~/.local/share/skykeep-mcp
```

Configure your MCP client to start that command. Stdout carries protocol messages only. JSON-RPC batches are not supported and get an `Invalid Request` error. Several clients can share one directory: operations take turns on the lock, and a server that starts while another holds it still answers `initialize`, then returns `state_busy_retry` from each tool call until the lock is free. Setup prints the exact line to paste. The server never opens a keyring unlock dialog or a sign-in prompt; if the keyring is locked or the session has expired, tools return `login_required` with instructions, and with a protected file that is not unlocked they return `locked: run skykeep-mcp unlock in a terminal`. `auth_status` names the folder in that command, and says which other problem a protected file has, such as a damaged file or a blocked kernel key storage. While running it refreshes Notes and Reminders every five minutes; a failed pass waits for the next interval rather than retrying.

## Tools

| Tool | What it does |
|---|---|
| `auth_status` | Whether the folder is bound to an account, whether the protected file is locked, and how to sign in. Takes no arguments and never accepts secrets. |
| `sync` | Refreshes Notes and Reminders, then sends queued changes. Nothing is sent when the refresh fails. |
| `sync_status` | Last refresh per service, whether the last pass failed, and counts of pending and refused changes. No network access. |
| `notes_list`, `notes_folders`, `notes_search`, `notes_get` | Read the local cache. `notes_get` returns the plain text and the native formatting runs. |
| `notes_write` | Queues `create`, `save`, `move`, `pin` or `delete`. Edits to an existing note need the `base_etag` you read, and are refused for locked or read-only notes. |
| `reminders_lists`, `reminders_list`, `reminders_search`, `reminders_get` | Read the local cache. |
| `reminders_write` | Queues `create`, `edit`, `tick` (complete or reopen) or `delete`. An `edit` changes only the fields in `patch`; moving a reminder is an edit of `list_guid`. |
| `contacts_list`, `contacts_search`, `contacts_get` | Read the local Contacts cache. Read-only. |
| `contacts_refresh` | Reads Contacts from Apple into the local cache, replacing it. Never writes to Apple and never opens a sign-in prompt. |
| `calendar_calendars`, `calendar_events`, `calendar_search`, `calendar_get` | Read the local Calendar cache. Read-only. |
| `calendar_refresh` | Reads Calendar from Apple into the local cache over a bounded window, replacing it. Never writes to Apple, never answers invitations and never opens a sign-in prompt. |
| `photos_albums`, `photos_list`, `photos_search`, `photos_get` | Read the cached metadata of your personal Photos library: dates, kind, size, filename, title, caption, keywords. No pictures, places or faces. Read-only. |
| `photos_refresh` | Reads Photos metadata from Apple into the local cache, a bounded batch per call, so a large library takes several calls. Never downloads pictures and never changes photos or albums. |
| `drive_list`, `drive_get`, `drive_search` | List a cached iCloud Drive folder, read one item's details, or search names. Paths look like `/Documents/Taxes`. Metadata only: no file contents, no downloads, no local paths. Read-only. |
| `drive_refresh` | Reads one Drive folder's item list (and those on its path) from Apple into the local cache. Never changes anything in Drive. |

Contacts, Calendar, Photos and Drive start empty: call their `*_refresh` tool once to fill the cache, and again whenever you want newer data. Their refresh is not part of `sync`.

Reads come from the local cache and say so (`"source": "local_cache"`). A successful write means the change is queued in the library's durable outbox; `remote_verified` is always `false`, because the server does not read each field back from iCloud after sending.

## Write approval

`setup` sets the approval mode for you (step 4 under "Get started"). The commands in this section apply only to a folder with no approval mode: one where you chose 4 in setup, or one set up before approval modes existed. In a folder with mode 1, 2 or 3, approval happens in your MCP client, and `require-approval` and `allow-routine` refuse to run.

Deletions never run straight from MCP. A deletion here means `delete`, a `move` into Recently Deleted (that is how iCloud deletes a note), and a `save` that leaves the note body empty. Creating a note in Recently Deleted is refused. For any of these the tool returns `approval_required` with a request ID, and you approve in your own terminal:

```sh
skykeep-mcp approve --state-dir ~/.local/share/skykeep-mcp
```

It asks for the request ID, shows the exact queued operation and queues it only after you type `approve`. To require this for every write, run `skykeep-mcp require-approval --state-dir DIR`; `allow-routine` switches back. Both commands ask you to type `confirm`.

There is no volume limit. Under the default policy an agent can queue any number of routine edits, ticks and saves that shorten a note without emptying it. If you want to see every write first, use `require-approval`.

What approval protects against: an agent that reaches this server only through MCP. It cannot approve its own requests, change the policy or sign in, because none of that is exposed as a tool. It does not protect against an agent that can run shell commands as your user. Such an agent can clear the marker variables and run `approve` or `allow-routine` in a pseudo-terminal, or edit the library file directly. The agent-marker check in `login` and the helper commands is a guard against accidents, not a security boundary. Don't give an agent shell access to the account that owns the state directory if you rely on approval.

## Limitations

### Approval modes in detail

Setup's choices 1 to 3 are the policy modes `reads_and_writes`, `writes_only` and
`yolo` (the table is under "Get started"). They control reads, Notes and Reminders
changes, and sync. A folder with no policy (choice 4) keeps the older behaviour in
"Write approval" above. MCP clients that can't answer the approval forms, Hermes
among them today (see below), need choice 4.

The human-only command is `skykeep-mcp policy --state-dir DIR MODE`, where MODE is
`reads_and_writes`, `writes_only`, or `yolo`. It requires a user-owned terminal,
literal `confirm`, persists a fresh revision bound to the account and directory
device/inode, and prints the saved record. It is never exposed as an MCP tool.
Account changes or copied policies fail closed until the human explicitly migrates
again. Existing terminal policy commands cannot override an activated new policy.
No migration back to legacy is provided. In each mode that asks, every approval is
a one-shot form in the MCP client.

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
terminal proposals. It runs however large a legacy queue is. Every new enqueue
records the exact native payload and policy revision in the same transaction.
Coalescing requires previously authorized input and replaces the resulting payload
grant. Tries can change during retry; operation bytes cannot. Changing policy
quarantines even previously approved pending writes.

A grant binds only the cached rows the native sender reads for that operation:
the item itself, a note's body for `save` (runs come from the payload), the subtasks for a reminder
edit that moves lists, and the notes template for a note `create`. Lists, folders
and other items are not bound, so refreshing them does not stale a grant. When an
approved write changes the cached rows of another queued write (a subtask edit
under a parent's list move, or a parent move that rewrites its subtasks), that
write is re-bound in the same transaction, but only if its grant was current just
before. An approved deletion that removes an item, or its subtasks, refuses older
writes to those items with `superseded_by_approved_deletion`. A changed payload
on another record is never re-authorized.

Before session validation, sync checks read permission and the policy binding. In
`reads_and_writes`, startup/timer refresh is refused without issuing a form;
only an originating `sync` call can request one-shot refresh consent. The other
modes permit background refresh. A queued write whose grant is missing, stale,
oversized (including a bound item that grew past the serializer limit) or whose
item is gone is refused alone with the reason `mcp_quarantine:<code>` and is never
sent; the rest of the queue continues. The cache still shows a quarantined write as
if it were on its way, so after its drain the same sync re-reads that service in
full. A full pass replays only waiting writes, which drops the quarantined change
from the cache. If that pass fails, the next sync that reaches the drain tries
again. Until it succeeds, reads can still show the unsent change.
Native sends recheck exact payload, account, policy, directory and the bound cached
rows after refresh, before entering the writer, and refuse that write if they
changed. A sync has a 60-second async deadline. At most 128 waiting writes are
admitted. Timeout after a send begins means delivery may be uncertain; the
original durable record remains for retry. These safeguards can refuse delivery
after a concurrent remote edit of the same item. Apple-side conflict/merge
behavior remains native engine behavior; local acceptance is not field-level
remote verification.

The `sync` reply reports `queued` (waiting writes only), `refused` (all kept
refused writes) and `refused_this_sync` with each write this sync refused or
quarantined. `sync_status` reports per service the refused count and the 20 newest
as `refused_recent`. Both show id, service, kind, record and a reason code, never
payloads; engine and Apple reasons appear only as `refused_by_sync_engine`. A
sync that fails after its local gate does not list its quarantines; `sync_status`
does.

Refused writes stay in the private directory and `sync_status` counts them. They
do not count toward the 128 waiting writes. Up to 1024 refused writes are kept;
after that new writes fail with `quarantine_full_retire_in_terminal`. The
human-only `skykeep-mcp quarantine --state-dir DIR` lists them in a user-owned
terminal (id, service, kind, record, reason; no payloads) and deletes exactly the
listed rows after the literal answer `retire`. Retired writes are never sent. No
MCP tool can resend, release or retire a refused write.

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
- Only Linux is supported. The sign-in is kept in a Secret Service keyring or in the passphrase-protected file; with neither, setup and sign-in stop instead of keeping it in a plain file.
- The protected file needs the Linux kernel key storage for `unlock`. Some containers block it (Docker's default security profile does); there `unlock` says so and the server stays locked.