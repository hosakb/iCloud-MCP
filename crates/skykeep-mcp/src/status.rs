//! `skykeep-mcp status`: what is set up in a folder, read on this computer only. No network,
//! no sign-in prompt, no tokens or private content in what it prints.
use crate::storage::{self, Closed, Env, KeyringState, Kind};
use std::path::Path;

/// The approval modes, strictest first, with the sentence setup and status show for each.
pub const MODES: [(&str, &str); 3] = [
    (
        "reads_and_writes",
        "Ask me before every read and every change.",
    ),
    (
        "writes_only",
        "Read without asking, but ask me before every change.",
    ),
    (
        "yolo",
        "Don't ask before reads or changes. Deletions, moves to Recently Deleted and moves and renames of large Drive folders still ask on the approval card.",
    ),
];

pub fn describe_mode(mode: &str) -> &'static str {
    MODES
        .iter()
        .find(|(m, _)| *m == mode)
        .map_or("an unknown mode", |(_, text)| text)
}

/// One finding: fine, or a problem with the one next step that fixes it.
pub struct Line {
    pub text: String,
    pub next: Option<String>,
}

#[derive(Default)]
pub struct Report {
    pub lines: Vec<Line>,
    /// The folder checked, named in every command the report prints.
    pub dir: Option<std::path::PathBuf>,
}

/// Library key: setup chose to leave the folder without an approval mode. Only setup and status
/// read it; it changes no policy.
pub const NO_MODE_KEY: &str = "mcp.setup.no_approval_mode";

pub(crate) fn quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@".contains(c))
    {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// What "no approval mode" means in a folder: the older behaviour, including an earlier
/// `require-approval`, which setup never loosens.
pub fn no_mode_effect(library: &quince_core::Library) -> &'static str {
    if require_all(library) {
        "Every change asks on the approval card in your MCP client, because this folder was set earlier to approve every change (skykeep-mcp require-approval)."
    } else {
        "Routine changes run without asking; deletions and renames or moves of large Drive folders ask on the approval card in your MCP client."
    }
}

/// Setup's choice 4 in words: how it differs from yolo in this folder.
pub fn no_mode_choice(library: &quince_core::Library) -> &'static str {
    if require_all(library) {
        no_mode_effect(library)
    } else {
        "Like 3, but you can later run skykeep-mcp require-approval to make every change ask."
    }
}

fn require_all(library: &quince_core::Library) -> bool {
    library
        .state("mcp.require_write_approval")
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// `text` with `--state-dir DIR` added to every `skykeep-mcp` command in it. For the default
/// folder, only setup, status and unlock go without it: the other commands need the flag.
pub fn with_dir(text: &str, dir: &Path) -> String {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let default = crate::setup::default_dir().is_some_and(|d| canonical(&d) == canonical(dir));
    let flag = format!(" --state-dir {}", quoted(dir));
    let mut out = String::with_capacity(text.len() + flag.len());
    let mut rest = text;
    while let Some(at) = rest.find("skykeep-mcp ") {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let after = &from["skykeep-mcp ".len()..];
        let word: String = after
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        let known = [
            "setup",
            "status",
            "unlock",
            "login",
            "logout",
            "approve",
            "quarantine",
            "permissions",
            "exchange",
        ]
        .contains(&word.as_str());
        // `permissions show` is one command: the folder goes after both words.
        let sub = if word == "permissions" && after[word.len()..].starts_with(" show") {
            " show"
        } else {
            ""
        };
        let tail = &after[word.len() + sub.len()..];
        out.push_str("skykeep-mcp ");
        out.push_str(&word);
        out.push_str(sub);
        let knows_default = ["setup", "status", "unlock", "permissions"].contains(&word.as_str());
        if known && !(default && knows_default) && !tail.starts_with(" --state-dir") {
            out.push_str(&flag);
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

impl Report {
    fn ok(&mut self, text: impl Into<String>) {
        self.lines.push(Line {
            text: text.into(),
            next: None,
        });
    }
    fn fix(&mut self, text: impl Into<String>, next: impl Into<String>) {
        self.lines.push(Line {
            text: text.into(),
            next: Some(next.into()),
        });
    }

    /// Nothing needs fixing.
    pub fn ready(&self) -> bool {
        self.lines.iter().all(|l| l.next.is_none())
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match &line.next {
                None => out.push_str(&format!("  OK   {}\n", line.text)),
                Some(next) => {
                    out.push_str(&format!("  FIX  {}\n       Next step: {next}\n", line.text))
                }
            }
        }
        match &self.dir {
            Some(dir) => with_dir(&out, dir),
            None => out,
        }
    }
}

/// An Apple Account as status shows it: enough to recognise, not enough to read.
pub fn mask(account: &str) -> String {
    let account = account.trim();
    let first = |s: &str| s.chars().next().map(String::from).unwrap_or_default();
    match account.split_once('@') {
        Some((local, domain)) => {
            let tld = domain.rsplit_once('.').map(|(_, t)| t).unwrap_or("");
            let tld: String = tld
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(10)
                .collect();
            format!("{}***@{}***.{}", first(local), first(domain), tld)
                .chars()
                .filter(|c| !c.is_control())
                .collect()
        }
        None => {
            let digits: Vec<char> = account.chars().filter(char::is_ascii_digit).collect();
            let tail: String = digits[digits.len().saturating_sub(2)..].iter().collect();
            format!("***{tail}")
        }
    }
}

/// Everything status checks for `dir`. Takes the folder's lock while it reads.
pub fn check(env: &Env, dir: &Path) -> Report {
    let mut report = Report {
        dir: Some(dir.to_path_buf()),
        ..Report::default()
    };
    let shown = dir.display().to_string();
    if !dir.exists() {
        report.fix(
            format!("No Skykeep folder at {shown}."),
            "Run skykeep-mcp setup.",
        );
        return report;
    }
    let held = match crate::state::PrivateState::open(dir) {
        Ok(held) => held,
        Err(error) => {
            let (text, next) = match error {
                "state_must_be_owned_and_private" => (
                    format!("The folder {shown} is not private to you."),
                    format!(
                        "Make sure it belongs to you, then run: chmod 700 {}",
                        quoted(dir)
                    ),
                ),
                "unsafe_state_contents" => match crate::state::unsafe_entry(dir) {
                    Some((path, true)) => (
                        format!("{} is a link inside the folder.", path.display()),
                        format!("Remove the link: rm {}", quoted(&path)),
                    ),
                    Some((path, false)) => (
                        format!("Others can read or change {}.", path.display()),
                        format!(
                            "Delete it if it is an editor's backup copy, or run: chmod 600 {}",
                            quoted(&path)
                        ),
                    ),
                    None => (
                        format!("A file in {shown} can be read by others, or is a link."),
                        format!(
                            "Run: chmod 600 {}/* and remove any links from the folder.",
                            quoted(dir)
                        ),
                    ),
                },
                "unsafe_state_path" => (
                    format!("The path {shown} goes through a link or '..'."),
                    "Use the folder's real path.".to_string(),
                ),
                "state_busy_retry" => (
                    "The server is using the folder right now.".to_string(),
                    "Run skykeep-mcp status again in a moment.".to_string(),
                ),
                _ => (
                    format!("The folder {shown} can't be opened."),
                    "Check that it exists and belongs to you.".to_string(),
                ),
            };
            report.fix(text, next);
            return report;
        }
    };
    let dir = held.path.as_path();
    report.dir = Some(dir.to_path_buf());
    report.ok(format!("Folder: {} (private to you)", dir.display()));

    let store = storage_lines(env, dir, &mut report);

    let library = if dir.join("library.db").exists() {
        quince_core::Library::open(dir).ok()
    } else {
        None
    };
    let session = match store.as_ref().map(|s| s.try_load()) {
        Some(Ok(session)) => Some(session),
        Some(Err(_)) => {
            // The storage answered a moment ago; a failed read now is still a problem.
            report.fix(
                "Skykeep couldn't read the saved sign-in from its storage.",
                "Run skykeep-mcp status again in a moment. If it keeps failing, run skykeep-mcp setup.",
            );
            None
        }
        None => None,
    };
    match &session {
        Some(s) if s.is_resumable() => report.ok(format!(
            "Signed in as {} (checked on this computer only)",
            s.apple_id
                .as_deref()
                .map(mask)
                .unwrap_or_else(|| "your Apple Account".into())
        )),
        Some(s) if s.apple_id.is_some() => report.fix(
            "The saved sign-in has expired.",
            "Run skykeep-mcp setup to sign in again.",
        ),
        Some(_) => report.fix("Not signed in.", "Run skykeep-mcp setup."),
        None => {}
    }

    let bound = library
        .as_ref()
        .and_then(|l| l.state(quince_sync::queue::ACCOUNT_KEY).ok().flatten());
    match (&bound, session.as_ref().and_then(|s| s.dsid.as_deref())) {
        (None, _) => report.fix(
            "This folder is not tied to an Apple Account yet.",
            "Run skykeep-mcp setup.",
        ),
        (Some(tag), Some(dsid)) if *tag != quince_sync::queue::account_tag(dsid) => report.fix(
            "This folder belongs to a different Apple Account than the saved sign-in.",
            "Use a new folder for the other account: skykeep-mcp setup --state-dir NEW_FOLDER",
        ),
        (Some(_), _) => report.ok("The folder is tied to one Apple Account."),
    }

    match library.as_ref().map(|l| crate::policy(l, dir)) {
        Some(Ok(Some((policy, _)))) => {
            report.ok(format!("Approval mode: {}", describe_mode(&policy.mode)))
        }
        Some(Ok(None))
            if library
                .as_ref()
                .and_then(|l| l.state(NO_MODE_KEY).ok().flatten())
                .as_deref()
                == Some("1") =>
        {
            report.ok(format!(
                "Approval mode: none. {}",
                library.as_ref().map(no_mode_effect).unwrap_or_default()
            ))
        }
        None | Some(Ok(None)) => report.fix(
            "No approval mode chosen.",
            "Run skykeep-mcp setup to choose one.",
        ),
        Some(Err("policy_account_changed")) => report.fix(
            "The approval mode was set before sign-in or for another account.",
            "Run skykeep-mcp setup to choose it again.",
        ),
        Some(Err("policy_state_changed")) => report.fix(
            "The approval mode belongs to another folder (this one was copied or moved).",
            "Run skykeep-mcp setup to choose it again.",
        ),
        Some(Err(_)) => report.fix(
            "The approval mode setting can't be read.",
            "Run skykeep-mcp setup to choose it again.",
        ),
    }
    // Read only: status never records a load; the server does that at its next tool call.
    if let Some(library) = library.as_ref() {
        let mode = crate::policy(library, dir)
            .ok()
            .flatten()
            .map(|(p, _)| p.mode);
        for (text, next) in crate::permissions::status(library, dir, mode.as_deref()) {
            match next {
                Some(next) => report.fix(text, next),
                None => report.ok(text),
            }
        }
    }
    report
}

/// The storage lines, and the store when it can be read now.
fn storage_lines(env: &Env, dir: &Path, report: &mut Report) -> Option<quince_auth::Store> {
    let chosen = match storage::kind(dir) {
        Ok(chosen) => chosen,
        Err(error) => {
            report.fix(
                "The sign-in storage setting is damaged.",
                "Run skykeep-mcp setup.",
            );
            let _ = error;
            return None;
        }
    };
    match chosen.unwrap_or(Kind::SystemKeyring) {
        Kind::SystemKeyring => {
            let store = (env.keyring)(dir).ok();
            if store.as_ref().is_some_and(|s| s.sign_out_pending()) {
                report.fix(
                    "A sign-out is still waiting for the system keyring.",
                    "Unlock your keyring, then run skykeep-mcp logout again.",
                );
                return None;
            }
            match storage::keyring_state(env, dir) {
                KeyringState::Ready => {
                    report.ok("Sign-in storage: system keyring (unlocked)");
                    store
                }
                KeyringState::Locked => {
                    report.fix(
                        "Sign-in storage: system keyring, locked.",
                        "Unlock your keyring (log in to your desktop session), then run skykeep-mcp status again.",
                    );
                    None
                }
                KeyringState::Unknown => {
                    report.fix(
                        "Sign-in storage: system keyring, which answers but can't tell whether it is locked.",
                        "Log in to your desktop session, then run skykeep-mcp status again.",
                    );
                    None
                }
                KeyringState::Missing => {
                    report.fix(
                        "Sign-in storage: system keyring, not reachable.",
                        if env.holder.available() {
                            "Log in to your desktop session so the keyring starts, or run skykeep-mcp setup to use a protected file instead."
                        } else {
                            "Log in to your desktop session so the keyring starts. If you are inside Docker or a similar container, run Skykeep directly on the computer."
                        },
                    );
                    None
                }
            }
        }
        Kind::ProtectedFile => match storage::protected_state(env, dir) {
            Ok(()) => {
                report.ok("Sign-in storage: protected file (unlocked)");
                storage::open_store_in(env, dir).ok()
            }
            Err(Closed::Locked) => {
                report.fix(
                    "Sign-in storage: protected file, locked.",
                    "Run skykeep-mcp unlock.",
                );
                None
            }
            Err(Closed::Missing) => {
                report.fix(
                    "Sign-in storage: protected file, not created yet.",
                    "Run skykeep-mcp setup.",
                );
                None
            }
            Err(other) => {
                report.fix(
                    format!("Sign-in storage: protected file. {}", other.sentence()),
                    "Run skykeep-mcp setup.",
                );
                None
            }
        },
    }
}
