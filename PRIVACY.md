# Privacy

skykeep-mcp runs locally. It has no network listener, hosted account service, analytics or crash reporting. The MCP server syncs Notes and Reminders with Apple. The retained engine supports other services, but the MCP server does not sync or contact them.

## Local data

The private state directory contains the cached library, queued changes and account binding. It must be owned by you with mode `0700` and cannot contain symlinks or shared files. Notes and reminders in the SQLite cache are not encrypted by this server. Protect the account and disk that hold them.

The system keyring stores session and trust tokens. Cookies on disk are sealed using a key held in that keyring. Login does not store the account password. Credentials must be entered in your own controlling terminal, never through MCP, chat, command arguments or environment variables.

`logout --state-dir DIR` removes session credentials and the cookie file, not cached notes and reminders. If the keyring is locked, the server records sign-out and completes it when the keyring next answers; meanwhile it loads no session. Delete the state directory separately if you want to remove the cache.

## Data given to clients

MCP read tools return local note and reminder content to the client that invoked them. The client's model provider, logging and retention policies are outside this repository. Configure the client accordingly before granting access.

Writes first enter the local outbox. The next successful sync can send them to Apple. Under the default policy routine writes need no human approval and have no volume limit. Destructive writes require a trusted-terminal approval. Use `require-approval --state-dir DIR` to require approval for every write.

Approval does not protect local data from software running as the state owner. In particular, an agent with shell access can read the cache or bypass terminal-helper guards. See [SECURITY.md](SECURITY.md) and the [README](README.md).

Do not include account identifiers, tokens, note content or personal reminder data in public bug reports, logs or test fixtures.