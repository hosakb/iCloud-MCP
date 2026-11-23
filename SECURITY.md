# Security

## Scope and trust boundary

iCloud-MCP (the `skykeep-mcp` command) is a local stdio server for Notes, Reminders, Contacts, Calendar, Photos and iCloud Drive. There is no network listener. The MCP client receives the content requested through its tools and can queue routine edits under the default policy. Treat access to the server as access to the cached library and the permitted iCloud writes.

Terminal approval protects against MCP-only clients, not processes running as your user. Shell-capable agents can edit the database, clear agent-marker environment variables or invoke approval and policy helpers themselves. Do not give an agent shell access to the account holding state if you rely on approval.

## Credentials and state

- Enter passwords and verification codes only in a controlling terminal you opened yourself. Login hides input and rejects known agent-runtime markers; those checks prevent accidents, not malicious local access.
- Passwords are not stored by the MCP server. Session and trust tokens use the Linux Secret Service keyring; cookies are sealed with a key stored there. A locked keyring or expired session returns `login_required` without opening an unlock dialog.
- Each state directory is private, account-bound and exclusively locked for operations. It must be owned by the user with mode `0700`, without symlinks or shared files. The local SQLite cache is not encrypted by the server.
- The server uses its own keyring namespace and does not import desktop sessions. Logout removes credentials, not cached content. A locked keyring defers credential erasure while preventing session reuse.

## Writes

Deletes, moves into Recently Deleted and saves that empty a note require a terminal approval. Creating a note in Recently Deleted is refused. `require-approval --state-dir DIR` extends approval to every write; `allow-routine --state-dir DIR` restores the default. Neither policy change is available as an MCP tool.

There is no volume limit on routine writes. A successful tool response means queued locally. Even after delivery the server does not verify every remote field, and `remote_verified` remains `false`. Unsupported note structures, locked notes and read-only notes are refused rather than silently rewritten. See the [README](README.md) for operation semantics and inherited limitations.

## Development and reporting

Use synthetic fixtures and mock services for tests. Live Apple sign-in or writes require separate authorization and a test account. Never put secrets or personal library content in fixtures, logs or reports.

Report vulnerabilities privately to the maintainers of this fork. Use this repository's GitHub Security tab if private reporting is available. If it is unavailable, ask for a private contact without disclosing vulnerability details in a public issue. Do not send fork-specific reports to upstream by default.

This repository derives from [Skykeep](https://github.com/YashwanthKamireddi/Skykeep). Its retained engine code is not a claim that every upstream feature is exposed, tested against Apple or supported by this MCP server.