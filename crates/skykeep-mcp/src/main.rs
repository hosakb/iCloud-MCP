//! Newline-delimited stdio only. Credentials are never accepted on this channel.
use serde_json::json;
use skykeep_mcp::{login, state::PrivateState, terminal, Server};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // This executable owns its process. Newly created cache/state files are user-only.
    unsafe {
        libc::umask(0o077);
    }
    if let Err(error) = run().await {
        eprintln!("skykeep-mcp: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let mut args = std::env::args_os().skip(1).peekable();
    let command = if args.peek().is_some_and(|s| {
        [
            "login",
            "logout",
            "approve",
            "require-approval",
            "allow-routine",
        ]
        .iter()
        .any(|name| s == name)
    }) {
        args.next().unwrap().to_string_lossy().to_string()
    } else {
        "serve".into()
    };
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--state-dir")) {
        return Err("usage: skykeep-mcp [login|logout] --state-dir PRIVATE_DIRECTORY");
    }
    let dir = PathBuf::from(args.next().ok_or("state_directory_required")?);
    if args.next().is_some() {
        return Err("unexpected_argument");
    }
    if command != "serve" {
        // An agent-launched PTY can carry keystrokes through model-visible transcripts.
        if terminal::agent_marked(|name| std::env::var_os(name)) {
            return Err("user_owned_terminal_required");
        }
        // Check the user terminal before touching a keyring or any network.
        let mut tty = terminal::controlling()?;
        if command == "approve" {
            use std::io::Write;
            let id = terminal::hidden(&mut tty, "Approval request ID: ", Duration::from_secs(180))?;
            let mut server = Server::open(&dir)?;
            let details = server.approval_details(&id)?;
            writeln!(
                tty,
                "Review this exact queued operation:
{details}"
            )
            .map_err(|_| "terminal_unavailable")?;
            let answer = terminal::hidden(
                &mut tty,
                "Type approve to queue it, or Ctrl-C to cancel: ",
                Duration::from_secs(180),
            )?;
            if *answer != "approve" {
                return Err("approval_cancelled");
            }
            let result = server.approve(&id)?;
            eprintln!("{}", result["status"].as_str().unwrap_or("unknown"));
            return Ok(());
        }
        let held = PrivateState::open(&dir)?;
        if command == "require-approval" || command == "allow-routine" {
            let answer = terminal::hidden(
                &mut tty,
                "Type confirm to change write policy for this directory: ",
                Duration::from_secs(180),
            )?;
            if *answer != "confirm" {
                return Err("policy_change_cancelled");
            }
            let mut library =
                quince_core::Library::open(&held.path).map_err(|_| "library_unavailable")?;
            library
                .set_state(
                    "mcp.require_write_approval",
                    if command == "require-approval" {
                        "true"
                    } else {
                        "false"
                    },
                )
                .map_err(|_| "library_unavailable")?;
            eprintln!("Write policy updated. Deletions still require approval.");
            return Ok(());
        }
        let store =
            quince_auth::Store::isolated_keyring(&held.path).map_err(|_| "state_unavailable")?;
        if command == "logout" {
            store.clear().map_err(login::auth_error)?;
            if store.sign_out_pending() {
                return Err("logout_pending_unlock_keyring_and_retry");
            }
            eprintln!(
                "Signed out locally. Cached notes and reminders remain in the private directory."
            );
            return Ok(());
        }
        // Fail closed rather than replacing a session in an inaccessible keyring.
        store.try_load().map_err(login::auth_error)?;
        let mut client =
            quince_auth::Client::with(&held.path, quince_auth::Endpoints::default(), store)
                .map_err(login::auth_error)?;
        login::authenticate(&mut client, &mut |label| {
            terminal::hidden(&mut tty, label, Duration::from_secs(180))
        })
        .await?;
        login::bind_account(&held.path, &mut client)?;
        eprintln!("Signed in. The MCP server will reopen this session on its next sync request.");
        return Ok(());
    }
    let mut server = Server::open(&dir)?;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut schedule =
        skykeep_mcp::scheduler::Schedule::new(std::time::Instant::now(), Duration::from_secs(300));
    loop {
        let line = tokio::select! {
            line = input.next_line() => match line.map_err(|_| "transport_failed")? { Some(line) => line, None => break },
            _ = poll.tick() => {
                if schedule.due(std::time::Instant::now()) { let _ = server.refresh().await; }
                continue;
            }
        };
        let response = match serde_json::from_str(&line) {
            Ok(request) => server.handle(request).await,
            Err(_) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            ),
        };
        if let Some(response) = response {
            let mut bytes = serde_json::to_vec(&response).map_err(|_| "transport_failed")?;
            bytes.push(b'\n');
            output
                .write_all(&bytes)
                .await
                .map_err(|_| "transport_failed")?;
            output.flush().await.map_err(|_| "transport_failed")?;
        }
    }
    Ok(())
}
