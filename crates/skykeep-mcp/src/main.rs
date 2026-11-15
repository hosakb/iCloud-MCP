//! Newline-delimited stdio only. Credentials are never accepted on this channel.
use serde_json::json;
use skykeep_mcp::{login, state::PrivateState, terminal, Server};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};

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
            "policy",
            "quarantine",
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
    let policy_mode = if command == "policy" {
        Some(
            args.next()
                .ok_or("policy_mode_required")?
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };
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
        if let Some(mode) = policy_mode {
            use std::io::Write;
            writeln!(tty, "Set approval mode to {mode} for {}. YOLO includes deletion without prompts. All currently pending writes will be quarantined, including previously approved writes. Old terminal proposals cannot be released under this policy. Type confirm to persist.", dir.display()).map_err(|_| "terminal_unavailable")?;
            let answer = terminal::hidden(&mut tty, "Confirmation: ", Duration::from_secs(180))?;
            if *answer != "confirm" {
                return Err("policy_change_cancelled");
            }
            let saved = Server::open(&dir)?.set_policy(&mode)?;
            writeln!(tty, "Saved policy: {saved}").map_err(|_| "terminal_unavailable")?;
            return Ok(());
        }
        if command == "quarantine" {
            use std::io::Write;
            let mut server = Server::open(&dir)?;
            let rows = server.quarantined()?;
            if rows.is_empty() {
                writeln!(tty, "No quarantined writes.").map_err(|_| "terminal_unavailable")?;
                return Ok(());
            }
            for row in &rows {
                writeln!(tty, "{row}").map_err(|_| "terminal_unavailable")?;
            }
            writeln!(tty, "These {} writes were refused or quarantined and will never be sent. Retiring deletes exactly these rows from {} for good.", rows.len(), dir.display()).map_err(|_| "terminal_unavailable")?;
            let answer = terminal::hidden(
                &mut tty,
                "Type retire to delete them, anything else cancels: ",
                Duration::from_secs(180),
            )?;
            if *answer != "retire" {
                return Err("quarantine_retire_cancelled");
            }
            let ids: Vec<i64> = rows.iter().filter_map(|r| r["id"].as_i64()).collect();
            let retired = server.retire_quarantined(&ids)?;
            let left = server.quarantined()?.len();
            writeln!(tty, "Retired {retired} quarantined writes. {left} remain.")
                .map_err(|_| "terminal_unavailable")?;
            return Ok(());
        }
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
            let library =
                quince_core::Library::open(&held.path).map_err(|_| "library_unavailable")?;
            if library
                .state("mcp.policy")
                .map_err(|_| "policy_unavailable")?
                .is_some()
            {
                return Err("use_policy_command");
            }
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
    let mut input = BufReader::new(tokio::io::stdin());
    let mut frame = Vec::new();
    let mut output = tokio::io::stdout();
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut schedule =
        skykeep_mcp::scheduler::Schedule::new(std::time::Instant::now(), Duration::from_secs(300));
    loop {
        let line = tokio::select! {
            line = skykeep_mcp::read_frame(&mut input, &mut frame) => match line? { Some(line) => line, None => break },
            _ = poll.tick() => {
                if let Some(response) = server.expire_consent() {
                    let mut bytes = serde_json::to_vec(&response).map_err(|_| "transport_failed")?;
                    bytes.push(b'\n');
                    output.write_all(&bytes).await.map_err(|_| "transport_failed")?;
                    output.flush().await.map_err(|_| "transport_failed")?;
                }
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
