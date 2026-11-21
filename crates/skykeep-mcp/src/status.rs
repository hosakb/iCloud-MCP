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
    ("yolo", "Never ask, not even before deleting something."),
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
        out
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
    let mut report = Report::default();
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
                    format!("Make sure it belongs to you, then run: chmod 700 {shown}"),
                ),
                "unsafe_state_contents" => (
                    format!("A file in {shown} can be read by others, or is a link."),
                    format!("Run: chmod 600 {shown}/* and remove any links from the folder."),
                ),
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
    report.ok(format!("Folder: {} (private to you)", dir.display()));

    let store = storage_lines(env, dir, &mut report);

    let library = if dir.join("library.db").exists() {
        quince_core::Library::open(dir).ok()
    } else {
        None
    };
    let session = store.as_ref().and_then(|s| s.try_load().ok());
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
                        "Unlock your keyring (log in to your desktop), then run status again.",
                    );
                    None
                }
                KeyringState::Missing => {
                    report.fix(
                        "Sign-in storage: system keyring, not reachable.",
                        "Start your desktop's keyring, or run skykeep-mcp setup to use a protected file.",
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
