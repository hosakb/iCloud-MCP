//! This input belongs to a user-operated terminal, never to MCP stdin.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Environment markers set by agent runtimes for commands they launch.
pub const AGENT_MARKERS: [&str; 5] = [
    "HERMES_AGENT",
    "AI_AGENT",
    "HERMES_SESSION_ID",
    "CODEX_SANDBOX",
    "CLAUDECODE",
];

pub fn agent_marked(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    AGENT_MARKERS
        .iter()
        .any(|name| get(name).is_some_and(|v| !v.is_empty()))
}

pub fn controlling() -> Result<File, &'static str> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "interactive_terminal_required")?;
    if unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } != unsafe { libc::getpgrp() } {
        return Err("foreground_terminal_required");
    }
    Ok(tty)
}

struct Restore(RawFd, libc::termios);
impl Drop for Restore {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1);
        }
    }
}

pub fn hidden(
    tty: &mut File,
    label: &str,
    timeout: Duration,
) -> Result<Zeroizing<String>, &'static str> {
    let fd = tty.as_raw_fd();
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return Err("interactive_terminal_required");
    }
    let saved = unsafe { saved.assume_init() };
    let mut mode = saved;
    mode.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    mode.c_cc[libc::VMIN] = 1;
    mode.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) } != 0 {
        return Err("terminal_unavailable");
    }
    let _restore = Restore(fd, saved);
    tty.write_all(label.as_bytes())
        .map_err(|_| "terminal_unavailable")?;
    tty.flush().map_err(|_| "terminal_unavailable")?;
    let deadline = Instant::now() + timeout;
    // Room for the longest answer up front: a growing buffer would leave copies behind.
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    let result = loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break Err("input_timeout");
        };
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready == 0 {
            break Err("input_timeout");
        }
        if ready < 0 {
            break Err("input_cancelled");
        }
        let mut byte = [0];
        if tty.read(&mut byte).unwrap_or(0) != 1 {
            break Err("input_cancelled");
        }
        match byte[0] {
            b'\r' | b'\n' => {
                break String::from_utf8(bytes.to_vec())
                    .map(Zeroizing::new)
                    .map_err(|_| "invalid_input")
            }
            3 | 4 | 27 => break Err("input_cancelled"),
            8 | 127 => {
                bytes.pop();
            }
            b if b >= 32 && bytes.len() < 1024 => bytes.push(b),
            _ => break Err("invalid_input"),
        }
    };
    let _ = tty.write_all(b"\n");
    result
}

/// A line the person may see as they type: a menu choice or a folder, never a secret.
/// Ctrl-D cancels and the caller's timeout ends the wait.
pub fn visible(tty: &mut File, label: &str, timeout: Duration) -> Result<String, &'static str> {
    let fd = tty.as_raw_fd();
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return Err("interactive_terminal_required");
    }
    let saved = unsafe { saved.assume_init() };
    let mut mode = saved;
    // Ordinary line editing with echo. Ctrl-C stops the program as anywhere else; the mode
    // set here is the usual one, so nothing is left changed.
    mode.c_lflag |= libc::ECHO | libc::ICANON | libc::ECHOE | libc::ISIG;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) } != 0 {
        return Err("terminal_unavailable");
    }
    let _restore = Restore(fd, saved);
    tty.write_all(label.as_bytes())
        .map_err(|_| "terminal_unavailable")?;
    tty.flush().map_err(|_| "terminal_unavailable")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Err("input_timeout");
        };
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready == 0 {
            return Err("input_timeout");
        }
        if ready < 0 {
            return Err("input_cancelled");
        }
        let mut byte = [0];
        if tty.read(&mut byte).unwrap_or(0) != 1 {
            return Err("input_cancelled");
        }
        match byte[0] {
            b'\r' | b'\n' => {
                // Escape alone cancels; arrow keys and other escape sequences are dropped.
                if bytes == [27] {
                    return Err("input_cancelled");
                }
                return String::from_utf8(without_escapes(&bytes)).map_err(|_| "invalid_input");
            }
            3 | 4 => return Err("input_cancelled"),
            b if bytes.len() < 4096 => bytes.push(b),
            _ => return Err("invalid_input"),
        }
    }
}

/// A typed line without terminal escape sequences (arrow keys, Home, Delete) or other control
/// bytes.
pub fn without_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            27 => {
                i += 1;
                if matches!(bytes.get(i), Some(b'[') | Some(b'O')) {
                    i += 1;
                    // Parameters, then one final byte from @ to ~.
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                }
                i += 1;
            }
            b if b < 32 || b == 127 => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// The help a command prints for `COMMAND --help` and `help COMMAND`, `None` for a word that
/// is not a command. Each starts with the command's usage line.
pub fn command_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "setup" => {
            "usage: icloud-mcp setup [--state-dir FOLDER]

Guided setup in five steps: the folder, where the sign-in is kept, the Apple sign-in, the
approval mode and a check. Run it in a terminal you opened yourself. You can stop at any time
and run it again; it continues where it stopped. In a folder that is already set up it offers
to keep the setup, sign in again or switch to another Apple Account.
FOLDER defaults to ~/.local/share/icloud-mcp.
"
        }
        "status" => {
            "usage: icloud-mcp status [--state-dir FOLDER]

Checks this computer's setup for FOLDER: the folder, the sign-in storage, the approval mode,
the permissions file and the exchange folder. Uses no network and asks for no password. Each
problem comes with the next step, and the command exits with 1 while something needs fixing.
FOLDER defaults to ~/.local/share/icloud-mcp.
"
        }
        "unlock" => {
            "usage: icloud-mcp unlock [--state-dir FOLDER] [--hours N]

For a folder that keeps the sign-in in a protected file: after a restart, opens that file with
its passphrase so the MCP server can use the sign-in. Without --hours it stays open until the
computer restarts; with --hours N (1 to 8760) it closes again after N hours.
FOLDER defaults to ~/.local/share/icloud-mcp.
"
        }
        "permissions" => {
            "usage: icloud-mcp permissions show [--state-dir FOLDER]

Shows the rules from permissions.yaml that are in use, the approval mode and exchange folder
the file names, what changed since you last looked, and the rules saved from an approval card.
FOLDER defaults to ~/.local/share/icloud-mcp.
"
        }
        "exchange" => {
            "usage: icloud-mcp exchange [--state-dir FOLDER] [--path DIR] [--max-mb N]

Sets up the one folder that Drive and Photos uploads read from and Drive downloads write into,
and prints it. The flags can come in any order. Without --path it keeps the folder
permissions.yaml names, or else uses ~/iCloud-MCP-Exchange. --max-mb sets the size limit per
upload or download, from 1 to 10240 MB (100 MB unless you set it). The command writes
exchange_folder: into permissions.yaml, which you can also edit yourself.
FOLDER defaults to ~/.local/share/icloud-mcp.
"
        }
        "login" => {
            "usage: icloud-mcp login --state-dir FOLDER

Signs in to Apple again in a folder that is already set up, with the sign-in storage chosen in
setup. Run it in a terminal you opened yourself. Running setup again does the same and also
checks the rest of the folder.
"
        }
        "logout" => {
            "usage: icloud-mcp logout --state-dir FOLDER

Signs out on this computer and forgets the saved sign-in. The cached notes, reminders and other
data stay in the folder.
"
        }
        "policy" => {
            "usage: icloud-mcp policy --state-dir FOLDER MODE

Sets the approval mode. MODE is one of:
  reads_and_writes  every read and every change asks on the approval card
  writes_only       reads run; every change asks on the approval card
  yolo              reads and changes run without asking
In every mode, deletions, moves to Recently Deleted and renames or moves of large Drive folders
ask on the approval card. The command writes approval_mode: into permissions.yaml, which you can
also edit yourself. Changes waiting to be sent are set aside and never sent.
"
        }
        "approve" => {
            "usage: icloud-mcp approve --state-dir FOLDER

Only for actions set to ask: terminal in permissions.yaml. Asks for the request ID the AI app
was given, shows the exact change and waits for you to type approve. A Notes or Reminders
change is queued for the next sync; a Drive, Contacts, Photos or Calendar change runs at once
and can't be undone from here.
"
        }
        "quarantine" => {
            "usage: icloud-mcp quarantine --state-dir FOLDER

Lists the changes that were set aside or refused and will never be sent, then offers to delete
those rows for good: type retire to delete them.
"
        }
        "require-approval" => {
            "usage: icloud-mcp require-approval --state-dir FOLDER

For a folder without an approval mode: every change asks on the approval card. A folder with an
approval mode uses the policy command instead.
"
        }
        "allow-routine" => {
            "usage: icloud-mcp allow-routine --state-dir FOLDER

For a folder without an approval mode: routine changes run without asking again. Deletions
still ask on the approval card. A folder with an approval mode uses the policy command instead.
"
        }
        _ => return None,
    })
}

/// What an approval mode does, as the policy command says it before asking.
pub fn mode_meaning(mode: &str) -> &'static str {
    match mode {
        "reads_and_writes" => "Every read and every change asks on the approval card.",
        "writes_only" => "Reads run without asking; every change asks on the approval card.",
        _ => "Reads and changes run without asking.",
    }
}

/// The sentence a terminal command prints when it stops with `code`, naming `dir` in the
/// commands it shows. `None` for a code without one of its own.
pub fn sentence(code: &str, dir: &std::path::Path) -> Option<String> {
    let with = |text: &str| crate::status::with_dir(text, dir);
    let own = match code {
        "user_owned_terminal_required" => "This command refuses to run in an AI agent's terminal, because what you type there can end up in a transcript. Open your own terminal window and run it there.".to_string(),
        "interactive_terminal_required" | "terminal_unavailable" => "This command needs a real terminal. Open a terminal window and run it there, not through a pipe or another program.".to_string(),
        "foreground_terminal_required" => "This command must run in the foreground of your terminal. Run it again without & at the end.".to_string(),
        "state_directory_required" => "--state-dir needs a folder after it, for example: --state-dir ~/.local/share/icloud-mcp.".to_string(),
        "policy_mode_required" => "Name the approval mode after the folder: reads_and_writes, writes_only or yolo.".to_string(),
        "invalid_policy_mode" => "The approval mode must be reads_and_writes, writes_only or yolo. Nothing was changed.".to_string(),
        "policy_change_cancelled" => "Nothing was changed, because the answer was not confirm.".to_string(),
        "quarantine_retire_cancelled" => "Nothing was deleted, because the answer was not retire.".to_string(),
        "approval_cancelled" => "Nothing was approved, because the answer was not approve.".to_string(),
        "permissions_file_unusable" => with("permissions.yaml can't be edited, so nothing was changed. Run icloud-mcp permissions show to see what to fix."),
        "use_policy_command" => with("This folder has an approval mode, so this command changes nothing. Run icloud-mcp policy MODE to change the mode."),
        "unexpected_argument" => "The command got more words than it takes. Run it with --help to see its usage.".to_string(),
        "pending_changed" => "The waiting changes changed while you were asked, so nothing was changed. Run the same command again.".to_string(),
        "storage_already_chosen" => with("This folder already has its sign-in storage. Run icloud-mcp setup to see or change it."),
        "already_initialized" => "The server was already started in this connection.".to_string(),
        "exchange_limit_out_of_range" | "exchange_parent_missing" | "exchange_path_required" | "home_folder_unknown_give_path" => crate::exchange::explain(code, None, dir),
        "logout_pending_unlock_keyring_and_retry" => "The saved sign-in is still in the system keyring, because the keyring is locked. Unlock it (log in to your desktop session), then run the same command again.".to_string(),
        "invalid_request_id" | "approval_not_found" => "No waiting request has that ID. Copy the request ID from the AI app's reply exactly, then run the same command again.".to_string(),
        "approval_policy_changed" => "The approval mode, the account or the permissions file changed since the request was made, so it no longer counts. Ask the AI app to make the change again.".to_string(),
        _ => {
            let setup = crate::setup::plain(code);
            if setup != code {
                setup.to_string()
            } else {
                return crate::next_step(code, dir, &serde_json::Value::Null);
            }
        }
    };
    Some(own)
}

/// [`say`], with the code in brackets after the sentence when `error` is a code, so a script
/// or a search can still find it.
pub fn say_with_code(error: &str, dir: &std::path::Path) -> String {
    let said = say(error, dir);
    if error.contains(' ') || said.contains(error) {
        said
    } else {
        format!("{said} ({error})")
    }
}

/// [`sentence`], or one that names the code and says how to check the folder.
pub fn say(error: &str, dir: &std::path::Path) -> String {
    if error.contains(' ') {
        return error.to_string();
    }
    sentence(error, dir).unwrap_or_else(|| {
        crate::status::with_dir(
            &format!("The command stopped ({error}). Run icloud-mcp status to check the folder."),
            dir,
        )
    })
}

/// What the policy command says before it asks for confirm: the mode, what it does and what
/// happens to changes waiting to be sent.
pub fn policy_prompt(mode: &str, dir: &std::path::Path) -> String {
    format!(
        "Set the approval mode to {mode} for {}. {} In every mode, deletions, moves to Recently Deleted and renames or moves of large Drive folders still ask on the approval card. This writes approval_mode: {mode} into permissions.yaml there, the line iCloud-MCP uses. Changes waiting to be sent, including ones approved before, are set aside and never sent, and terminal requests made before can no longer be approved. Type confirm to save.",
        dir.display(),
        mode_meaning(mode)
    )
}
