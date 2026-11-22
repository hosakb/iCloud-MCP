//! Newline-delimited stdio only. Credentials are never accepted on this channel.
use serde_json::json;
use skykeep_mcp::{login, setup, state::PrivateState, storage, terminal, Server};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // This executable owns its process. Newly created cache/state files are user-only.
    unsafe {
        libc::umask(0o077);
    }
    let mut all: Vec<_> = std::env::args_os().skip(1).collect();
    let first = all.first().and_then(|c| c.to_str()).map(str::to_string);
    // Help after any command, too: `login --help` must not try to sign in.
    if matches!(first.as_deref(), Some("help")) || all.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return;
    }
    if first.as_deref() == Some("exchange") {
        all.remove(0);
        if let Err(error) = exchange(all) {
            eprintln!("skykeep-mcp: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(command) =
        first.filter(|c| ["setup", "status", "unlock", "permissions"].contains(&c.as_str()))
    {
        all.remove(0);
        if let Err(error) = guided(&command, all).await {
            eprintln!("skykeep-mcp: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run(all).await {
        eprintln!("skykeep-mcp: {error}");
        std::process::exit(1);
    }
}

const HELP: &str = "skykeep-mcp: an MCP server for Apple iCloud Notes, Reminders, Contacts, Calendar, Photos and Drive.

Start here, in a terminal window you opened yourself:
  skykeep-mcp setup [--state-dir FOLDER]    guided setup: folder, sign-in storage, Apple sign-in, approval mode
  skykeep-mcp status [--state-dir FOLDER]   check this computer's setup; no network, no passwords
  skykeep-mcp unlock [--state-dir FOLDER] [--hours N]
                                            after a restart, open the protected file with its passphrase
  skykeep-mcp permissions show [--state-dir FOLDER]
                                            the rules from permissions.yaml in use, what changed
                                            since you last looked, and deletions that ran without asking

More commands (each needs --state-dir FOLDER):
  skykeep-mcp login       sign in to Apple again
  skykeep-mcp logout      sign out and forget the saved sign-in
  skykeep-mcp policy --state-dir FOLDER MODE
                          set the approval mode: reads_and_writes, writes_only or yolo
  skykeep-mcp approve     approve one waiting change by its request ID: a Notes or
                          Reminders change is queued, a Drive change runs at once
  skykeep-mcp require-approval
                          folders without an approval mode: every change waits for approve
  skykeep-mcp allow-routine
                          folders without an approval mode: routine changes run again,
                          deletions still wait for approve
  skykeep-mcp quarantine  list and retire writes that were set aside
  skykeep-mcp exchange [--state-dir FOLDER] [--path DIR] [--max-mb N]  (flags in any order)
                          set up the one folder Drive uploads read from and downloads
                          write into (default ~/Skykeep-Exchange, limit 100 MB) and print it
  skykeep-mcp --state-dir FOLDER
                          run the MCP server on stdin and stdout (your MCP client starts this)

FOLDER defaults to ~/.local/share/skykeep-mcp for setup, status, unlock and exchange.
";

/// `exchange`: the flags in any order. Sets up the Drive exchange folder and prints it; each
/// refusal is one sentence saying what is wrong and what to type.
fn exchange(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    const USAGE: &str =
        "usage: skykeep-mcp exchange [--state-dir FOLDER] [--path DIR] [--max-mb N]";
    let (mut dir, mut path, mut mb) = (None, None, None);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next();
        if flag == "--state-dir" && dir.is_none() {
            dir = Some(PathBuf::from(value.ok_or(USAGE)?));
        } else if flag == "--path" && path.is_none() {
            path = Some(PathBuf::from(value.ok_or_else(|| {
                skykeep_mcp::exchange::explain(
                    "exchange_path_required",
                    None,
                    std::path::Path::new("FOLDER"),
                )
            })?));
        } else if flag == "--max-mb" && mb.is_none() {
            mb = Some(value.and_then(|v| v.to_str().and_then(|v| v.parse::<u64>().ok())));
        } else {
            return Err(USAGE.into());
        }
    }
    // The same default folder as setup, status and unlock.
    let dir = dir
        .or_else(skykeep_mcp::setup::default_dir)
        .ok_or("exchange can't find your home folder: add --state-dir FOLDER")?;
    let dir = std::path::absolute(dir).map_err(|_| USAGE)?;
    let explain = |code: &str, path: Option<&std::path::Path>| {
        skykeep_mcp::exchange::explain(code, path, &dir)
    };
    let mb = match mb {
        Some(Some(n)) => Some(n),
        Some(None) => return Err(explain("exchange_limit_out_of_range", None)),
        None => None,
    };
    if terminal::agent_marked(|name| std::env::var_os(name)) {
        return Err("this command refuses to run in an AI agent's terminal: open your own terminal window and run it there".into());
    }
    let mut tty = terminal::controlling().map_err(|_| {
        "this command needs a real terminal: open a terminal window and run it there".to_string()
    })?;
    let held = PrivateState::open(&dir).map_err(|e| explain(e, None))?;
    let mut library =
        quince_core::Library::open(&held.path).map_err(|_| explain("library_unavailable", None))?;
    let shown = path
        .as_ref()
        .and_then(|p| std::path::absolute(p).ok())
        .or_else(|| {
            skykeep_mcp::exchange::configured(&library)
                .ok()
                .flatten()
                .map(|e| e.path)
        })
        .or_else(skykeep_mcp::exchange::default_path);
    let exchange = skykeep_mcp::exchange::set_up(&mut library, path, mb)
        .map_err(|e| explain(e, shown.as_deref()))?;
    use std::io::Write;
    writeln!(
        tty,
        "Drive exchange folder: {}\nSize limit: {} MB per upload or download.",
        exchange.path.display(),
        exchange.max_bytes / (1024 * 1024)
    )
    .map_err(|_| "terminal_unavailable".to_string())?;
    Ok(())
}

/// The person's own terminal, as setup and unlock talk to it.
struct Tty(std::fs::File);

impl setup::Console for Tty {
    fn say(&mut self, text: &str) -> Result<(), &'static str> {
        use std::io::Write;
        writeln!(self.0, "{text}").map_err(|_| "terminal_unavailable")
    }
    fn ask(&mut self, label: &str) -> Result<String, &'static str> {
        terminal::visible(&mut self.0, label, Duration::from_secs(180))
    }
    fn secret(&mut self, label: &str) -> Result<zeroize::Zeroizing<String>, &'static str> {
        terminal::hidden(&mut self.0, label, Duration::from_secs(180))
    }
}

/// `setup`, `status`, `unlock` and `permissions show`: `[--state-dir DIR]`, and
/// `[--hours N]` for unlock.
async fn guided(command: &str, args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut dir = None;
    let mut hours = None;
    let mut show = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            print!("{HELP}");
            return Ok(());
        }
        if arg == "show" && command == "permissions" && !show {
            show = true;
        } else if arg == "--state-dir" && dir.is_none() {
            dir = Some(PathBuf::from(
                args.next().ok_or("state_directory_required")?,
            ));
        } else if arg == "--hours" && command == "unlock" && hours.is_none() {
            let n: u32 = args
                .next()
                .and_then(|a| a.to_str().and_then(|a| a.parse().ok()))
                .filter(|n| (1..=8760).contains(n))
                .ok_or("--hours takes a whole number of hours from 1 to 8760")?;
            hours = Some(n);
        } else {
            return Err(match command {
                "unlock" => "usage: skykeep-mcp unlock [--state-dir FOLDER] [--hours N]",
                "status" => "usage: skykeep-mcp status [--state-dir FOLDER]",
                "permissions" => "usage: skykeep-mcp permissions show [--state-dir FOLDER]",
                _ => "usage: skykeep-mcp setup [--state-dir FOLDER]",
            }
            .into());
        }
    }
    let dir = dir
        .map(|d| std::path::absolute(d).map_err(|_| "state_unavailable"))
        .transpose()?;
    let env = storage::env();
    if command == "permissions" {
        if !show {
            return Err("usage: skykeep-mcp permissions show [--state-dir FOLDER]".into());
        }
        let dir = dir
            .or_else(setup::default_dir)
            .ok_or("permissions can't find your home folder: add --state-dir FOLDER")?;
        // Only the person's own terminal marks the changes as seen: an agent that runs this
        // command can't clear the notice status shows.
        let person = !terminal::agent_marked(|name| std::env::var_os(name))
            && unsafe { libc::isatty(1) } == 1;
        let text = skykeep_mcp::permissions::show(&dir, person)
            .map_err(|e| skykeep_mcp::permissions::plain(e).to_string())?;
        print!("{}", skykeep_mcp::status::with_dir(&text, &dir));
        return Ok(());
    }
    if command == "status" {
        let dir = match dir.or_else(setup::default_dir) {
            Some(dir) => dir,
            None => return Err("status can't find your home folder: add --state-dir FOLDER".into()),
        };
        let report = skykeep_mcp::status::check(env, &dir);
        print!("{}", report.render());
        if !report.ready() {
            std::process::exit(1);
        }
        println!("Everything is ready.");
        return Ok(());
    }
    if terminal::agent_marked(|name| std::env::var_os(name)) {
        return Err("this command refuses to run in an AI agent's terminal, because what you type there can end up in a transcript: open your own terminal window and run it there".into());
    }
    if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
        return Err(if command == "setup" {
            "setup needs a real terminal: open a terminal window and run skykeep-mcp setup there, not through a pipe or another program"
        } else {
            "unlock needs a real terminal: open a terminal window and run skykeep-mcp unlock there, not through a pipe or another program"
        }
        .into());
    }
    // Passwords, codes and passphrases pass through this process: no core dumps of it, and
    // no other process of this user may read its memory through ptrace.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let mut tty = Tty(terminal::controlling().map_err(|_| {
        "this command needs a real terminal: open a terminal window and run it there"
    })?);
    if command == "unlock" {
        let dir = dir
            .or_else(setup::default_dir)
            .ok_or("unlock can't find your home folder: add --state-dir FOLDER")?;
        return setup::unlock(&mut tty, env, &dir, hours);
    }
    let options = setup::Options {
        state_dir: dir,
        default_dir: setup::default_dir(),
        endpoints: quince_auth::Endpoints::default(),
        command: std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("skykeep-mcp")),
    };
    match setup::run(&mut tty, env, &options).await? {
        setup::Outcome::Ready(_) => Ok(()),
        setup::Outcome::Quit => {
            use setup::Console;
            tty.say("Setup stopped. Run skykeep-mcp setup again at any time.")
                .map_err(String::from)
        }
    }
}

async fn run(all: Vec<std::ffi::OsString>) -> Result<(), &'static str> {
    let mut args = all.into_iter().peekable();
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
        return Err("usage: skykeep-mcp [setup|status|unlock] or skykeep-mcp [login|logout] --state-dir PRIVATE_DIRECTORY");
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
            writeln!(tty, "Set approval mode to {mode} for {}. YOLO runs Notes and Reminders deletions without prompts; Drive deletions, and moves and renames of large Drive folders, still wait for skykeep-mcp approve. All currently pending writes will be quarantined, including previously approved writes. Old terminal proposals cannot be released under this policy. Type confirm to persist.", dir.display()).map_err(|_| "terminal_unavailable")?;
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
            let details = server.approval_text(&id)?;
            writeln!(
                tty,
                "Review this exact change:
{details}"
            )
            .map_err(|_| "terminal_unavailable")?;
            let answer = terminal::hidden(
                &mut tty,
                "Type approve to allow it (a Notes or Reminders change is queued for the next sync, a Drive change runs now), or Ctrl-C to cancel: ",
                Duration::from_secs(180),
            )?;
            if *answer != "approve" {
                return Err("approval_cancelled");
            }
            let result = match server.approve_any(&id).await {
                Ok(result) => result,
                Err(error) => {
                    if let Some(step) = skykeep_mcp::next_step(error, &dir, &json!(null)) {
                        writeln!(tty, "{step}").map_err(|_| "terminal_unavailable")?;
                    }
                    return Err(error);
                }
            };
            let status = result["status"]
                .as_str()
                .or_else(|| result["outcome"].as_str())
                .unwrap_or("unknown");
            match result["checked"].as_str() {
                Some(checked) => eprintln!("{status}: {checked}"),
                None => eprintln!("{status}"),
            }
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
        let store = storage::open_store(&held.path)?;
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
