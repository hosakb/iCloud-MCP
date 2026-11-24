//! `skykeep-mcp setup` and `skykeep-mcp unlock`: the guided, re-runnable first-time flow.
//!
//! Every answer comes from the person's own terminal through [`Console`]. Passwords, codes and
//! passphrases only ever go through [`Console::secret`]; nothing typed there is shown, kept in
//! a file, or put in any message. Each step is saved as soon as it is done, so a setup that
//! stops part way picks up where it left off.
use crate::login::{self, Echo};
use crate::state::PrivateState;
use crate::status::{self, MODES, NO_MODE_KEY};
use crate::storage::{self, Closed, Env, KeyringState, Kind};
use quince_auth::{Client, Endpoints};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// The person's terminal, as setup talks to it.
pub trait Console {
    fn say(&mut self, text: &str) -> Result<(), &'static str>;
    /// An answer shown as it is typed: a menu choice or a folder.
    fn ask(&mut self, label: &str) -> Result<String, &'static str>;
    /// An answer never shown: a password, a code or a passphrase.
    fn secret(&mut self, label: &str) -> Result<Zeroizing<String>, &'static str>;
}

/// A console that names the chosen folder in every `skykeep-mcp` command it prints.
struct Scoped<'a> {
    inner: &'a mut dyn Console,
    dir: Option<PathBuf>,
}

impl Scoped<'_> {
    fn scope(&self, text: &str) -> String {
        match &self.dir {
            Some(dir) => status::with_dir(text, dir),
            None => text.to_string(),
        }
    }
}

impl Console for Scoped<'_> {
    fn say(&mut self, text: &str) -> Result<(), &'static str> {
        let text = self.scope(text);
        self.inner.say(&text)
    }
    fn ask(&mut self, label: &str) -> Result<String, &'static str> {
        let label = self.scope(label);
        self.inner.ask(&label)
    }
    fn secret(&mut self, label: &str) -> Result<Zeroizing<String>, &'static str> {
        let label = self.scope(label);
        self.inner.secret(&label)
    }
}

pub struct Options {
    /// The folder given with `--state-dir`, if any.
    pub state_dir: Option<PathBuf>,
    /// The folder suggested when none was given.
    pub default_dir: Option<PathBuf>,
    pub endpoints: Endpoints,
    /// This program, for the client configuration lines.
    pub command: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready(PathBuf),
    Quit,
}

const NO_SECURE_STORAGE: &str = "Setup can't find a safe place on this computer to keep your sign-in. If you are inside Docker or a similar container, run iCloud-MCP directly on the computer. On a desktop, log in to the desktop session first, then run skykeep-mcp setup again.";
const UNKNOWN_KEYRING: &str = "Setup can't tell whether your system keyring is unlocked. Log in to your desktop session, then run skykeep-mcp setup again.";
const LOCKED_KEYRING: &str = "Your system keyring is locked. Unlock it (log in to your desktop session), then run skykeep-mcp setup again.";

/// `~/.local/share/skykeep-mcp`.
pub fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/skykeep-mcp"))
}

/// A sentence for each error code the steps below can stop with. Codes never carry secrets.
pub fn plain(code: &str) -> &str {
    match code {
        "credentials_rejected" => "Apple didn't accept that Apple Account or password. Run skykeep-mcp setup again to retry.",
        "code_incorrect_or_expired" => "The verification code was wrong or expired three times. Run skykeep-mcp setup again to get a new code.",
        "auth_cooldown_try_later" => "Apple asked to wait before the next sign-in attempt. Try again later.",
        login::SIGN_IN_WAIT => "iCloud-MCP is waiting before the next sign-in so Apple doesn't lock the account. Run skykeep-mcp setup again later.",
        "apple_answer_unreadable" => "Apple sent an answer iCloud-MCP couldn't read. Wait a minute, then run skykeep-mcp setup again.",
        "account_locked_visit_account_website" => "Apple has locked this Apple Account. Unlock it at iforgot.apple.com, then run skykeep-mcp setup again.",
        "secure_storage_unavailable" => "The sign-in could not be saved in the chosen storage. Run skykeep-mcp status to see why.",
        "regional_login_retry_required" => "Apple asked to sign in again. Run skykeep-mcp setup again.",
        "account_mismatch_use_new_state_directory" => "This folder belongs to a different Apple Account. Run setup again and choose a new folder for this account.",
        "security_key_sign_in_unsupported" => "This account signs in with a security key, which iCloud-MCP can't use yet.",
        "too_many_code_requests_try_later" => "Setup already asked Apple for several codes. Wait a while, then run skykeep-mcp setup again.",
        "input_cancelled" => "Setup stopped. Run skykeep-mcp setup again to continue where you left off.",
        "input_timeout" => "Setup waited three minutes without an answer and stopped. Run skykeep-mcp setup again to continue.",
        "invalid_input" => "That answer had characters setup can't read. Run skykeep-mcp setup again and type plain text.",
        "state_busy_retry" => "The server is using the folder right now. Run skykeep-mcp setup again in a moment.",
        "state_must_be_owned_and_private" => "That folder is not private to you. Choose another folder, or make it yours and run chmod 700 on it.",
        "unsafe_state_contents" => "A file in that folder can be read by others, or is a link. Choose another folder.",
        "unsafe_state_path" => "That folder path goes through a link or '..'. Use the folder's real path.",
        "state_unavailable" | "library_unavailable" => "Setup can't open that folder. Check that its parent folder exists and belongs to you.",
        "permissions_file_unavailable" => "Setup couldn't write the permissions file. Run skykeep-mcp status to see why.",
        "policy_unavailable" => "Setup couldn't read or save the approval mode. Run skykeep-mcp status to see why.",
        "authentication_unavailable" => "Sign-in didn't finish. Check your internet connection, then run skykeep-mcp setup again.",
        "login_incomplete" | "login_required" => "Sign-in didn't finish. Run skykeep-mcp setup again.",
        "foreground_terminal_required" => "Setup must run in the foreground of your terminal. Run it again without & at the end.",
        "interactive_terminal_required" | "terminal_unavailable" => "Setup lost the terminal. Run it again in a terminal window.",
        other => other,
    }
}

fn choose(
    console: &mut dyn Console,
    label: &str,
    count: usize,
    default: Option<usize>,
) -> Result<usize, &'static str> {
    let mut retry = false;
    loop {
        let answer = console.ask(&if retry {
            format!("Please type a number from 1 to {count}.\n{label}")
        } else {
            label.to_string()
        })?;
        let answer = answer.trim();
        if answer.is_empty() {
            if let Some(default) = default {
                return Ok(default);
            }
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=count).contains(&n) => return Ok(n),
            _ => retry = true,
        }
    }
}

/// A typed folder: spaces around it and one pair of matching quotes removed, `~` expanded.
/// `Err` holds the sentence for a path that is not absolute.
fn folder_answer(answer: &str) -> Result<PathBuf, String> {
    let mut text = answer.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = text[1..text.len() - 1].trim();
            break;
        }
    }
    let home = || {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    };
    if text == "~" {
        return home().ok_or_else(|| "Setup can't find your home folder.".to_string());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home()
            .map(|h| h.join(rest))
            .ok_or_else(|| "Setup can't find your home folder.".to_string());
    }
    if text.starts_with('/') {
        return Ok(PathBuf::from(text));
    }
    let here = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "the folder this terminal is in".into());
    Err(format!(
        "Type the full path, starting with / or ~. \"{text}\" would be a folder inside {here}, where this terminal is now."
    ))
}

/// Makes and checks the folder. Missing parents are made private to this user.
fn prepare(path: &Path) -> Result<PathBuf, &'static str> {
    use std::os::unix::fs::DirBuilderExt;
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("unsafe_state_path");
    }
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|_| "state_unavailable")?;
        }
    }
    let held = PrivateState::open(path)?;
    Ok(held.path.clone())
}

fn step_folder(console: &mut dyn Console, suggested: &Path) -> Result<PathBuf, &'static str> {
    console.say("\nStep 1 of 5: Folder\niCloud-MCP keeps its private data in one folder: its cache, its settings and the choice of where your sign-in is kept.")?;
    loop {
        let answer = console.ask(&format!(
            "Folder: {}\nPress Enter to use it, or type another folder: ",
            suggested.display()
        ))?;
        let path = if answer.trim().is_empty() {
            suggested.to_path_buf()
        } else {
            match folder_answer(&answer) {
                Ok(path) => path,
                Err(sentence) => {
                    console.say(&sentence)?;
                    continue;
                }
            }
        };
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .and_then(|h| PathBuf::from(h).canonicalize().ok());
        if home.is_some_and(|h| path.canonicalize().is_ok_and(|p| p == h)) {
            console.say("iCloud-MCP needs a folder of its own, not your home folder. Press Enter for the suggested folder, or type for example ~/.local/share/skykeep-mcp.")?;
            continue;
        }
        if !path.exists() {
            let create = console.ask(&format!("Create {}? [Y/n] ", path.display()))?;
            if matches!(create.trim().to_ascii_lowercase().as_str(), "n" | "no") {
                continue;
            }
        }
        match prepare(&path) {
            Ok(dir) => {
                console.say(&format!("Using {}.", dir.display()))?;
                return Ok(dir);
            }
            Err(code) => console.say(plain(code))?,
        }
    }
}

/// What is already done in a folder.
struct Found {
    storage_ready: bool,
    kind: Kind,
    account: Option<String>,
    /// The approval mode, or `none` when setup left the folder without one on purpose.
    mode: Option<String>,
}

impl Found {
    fn complete(&self) -> bool {
        self.storage_ready && self.account.is_some() && self.mode.is_some()
    }
}

fn no_mode_chosen(library: &quince_core::Library) -> bool {
    library.state(NO_MODE_KEY).ok().flatten().as_deref() == Some("1")
}

fn survey(env: &Env, dir: &Path) -> Result<Found, &'static str> {
    let held = PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let kind = storage::effective_kind(dir)?;
    let storage_ready = storage::kind(dir)?.is_some()
        && match kind {
            Kind::SystemKeyring => storage::keyring_state(env, dir) == KeyringState::Ready,
            Kind::ProtectedFile => storage::protected_state(env, dir).is_ok(),
        };
    let library = crate::setup(dir)?;
    let mut account = None;
    if storage_ready {
        let session = storage::open_store_in(env, dir)
            .ok()
            .and_then(|s| s.try_load().ok());
        if let Some(session) = session.filter(|s| s.is_resumable()) {
            let same = session.dsid.as_deref().is_some_and(|dsid| {
                crate::account::binding(&library, dsid) == Ok(crate::account::Binding::Same)
            });
            if same {
                account = Some(
                    session
                        .apple_id
                        .as_deref()
                        .map(status::mask)
                        .unwrap_or_else(|| "your Apple Account".into()),
                );
            }
        }
    }
    // The mode in force: the permissions file's approval_mode, else the stored one.
    let mode = match crate::policy(&library, dir) {
        Ok(_) if file_mode(&library, dir).is_some() => file_mode(&library, dir),
        // Taken over from a file line that is gone: back to no approval mode.
        Ok(Some(_)) if crate::adopted(&library) => Some("none".into()),
        Ok(Some((p, _))) => Some(p.mode),
        Ok(None) if no_mode_chosen(&library) => Some("none".into()),
        _ => None,
    };
    Ok(Found {
        storage_ready,
        kind,
        account,
        mode,
    })
}

/// The approval mode `permissions.yaml` sets in `dir` (or its last valid one), if it sets one.
fn file_mode(library: &quince_core::Library, dir: &Path) -> Option<String> {
    let stored = crate::stored_mode(library, dir).ok().flatten();
    crate::permissions::peek(library, dir, stored.as_deref())
        .ok()
        .filter(|a| a.mode_in_file)
        .and_then(|a| a.mode)
}

/// The mode in words; "none" says what it does in this folder, from `library`.
fn describe(mode: &str, library: &quince_core::Library) -> String {
    if mode == "none" {
        format!("none. {}", status::no_mode_effect(library))
    } else {
        status::describe_mode(mode).to_string()
    }
}

/// What Apple says about the saved sign-in.
enum Checked {
    Valid,
    Rejected,
    Unreachable,
}

/// Asks Apple whether the saved sign-in still works. Never asks for a password or a code.
async fn check_sign_in(env: &Env, dir: &Path, endpoints: &Endpoints) -> Checked {
    let Ok(held) = PrivateState::open(dir) else {
        return Checked::Unreachable;
    };
    let Ok(store) = storage::open_store_in(env, &held.path) else {
        return Checked::Unreachable;
    };
    let Ok(mut client) = Client::with(&held.path, endpoints.clone(), store) else {
        return Checked::Unreachable;
    };
    match client.resume().await {
        Ok(true) => Checked::Valid,
        Ok(false) => Checked::Rejected,
        Err(_) => Checked::Unreachable,
    }
}

fn next_free(dir: &Path) -> PathBuf {
    for n in 2..100 {
        let candidate = PathBuf::from(format!("{}-{n}", dir.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.with_file_name("skykeep-mcp-new")
}

/// One shell word: as it is when it holds only safe characters, else in single quotes.
fn shell(text: &str) -> String {
    let safe = !text.is_empty()
        && !text.starts_with('~')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:=@,~".contains(c));
    if safe {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// A YAML scalar: JSON's double-quoted string is valid YAML.
fn yaml(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// The whole guided flow. Every message names the chosen folder in its commands.
pub async fn run(
    console: &mut dyn Console,
    env: &Env,
    options: &Options,
) -> Result<Outcome, String> {
    let mut c = Scoped {
        inner: console,
        dir: None,
    };
    let outcome = flow(&mut c, env, options).await;
    outcome.map_err(|code| match code {
        login::SIGN_IN_WAIT => c.scope(&login::wait_text()),
        _ => c.scope(plain(code)),
    })
}

async fn flow(c: &mut Scoped<'_>, env: &Env, options: &Options) -> Result<Outcome, &'static str> {
    c.say("iCloud-MCP setup: five short steps. You can stop at any time with Ctrl-C and run setup again later; it continues where it stopped.")?;
    let suggested = options
        .state_dir
        .clone()
        .or_else(|| options.default_dir.clone())
        .ok_or("Setup can't find your home folder. Run it again with --state-dir FOLDER.")?;
    let mut dir = step_folder(c, &suggested)?;
    c.dir = Some(dir.clone());
    let mut again = false;
    let keep = loop {
        let found = survey(env, &dir)?;
        if !found.complete() {
            break false;
        }
        c.say("Checking the saved sign-in with Apple...")?;
        match check_sign_in(env, &dir, &options.endpoints).await {
            Checked::Valid => {}
            Checked::Rejected => {
                c.say("Apple no longer accepts the sign-in saved in this folder. Sign in again with the same Apple Account.")?;
                again = true;
                break false;
            }
            Checked::Unreachable => c.say(
                "Setup couldn't reach Apple to check the saved sign-in, so it is kept as it is.",
            )?,
        }
        let mode = {
            let held = PrivateState::open(&dir)?;
            let library = crate::setup(&held.path)?;
            let mut text = describe(found.mode.as_deref().unwrap_or_default(), &library);
            if file_mode(&library, &held.path).is_some() {
                text.push_str(" (set in permissions.yaml)");
            }
            text
        };
        c.say(&format!(
            "\nThis folder is already set up. Sign-in kept in the {}, signed in as {}, approval mode: {mode}",
            found.kind.describe(),
            found.account.as_deref().unwrap_or_default(),
        ))?;
        match choose(
            c,
            "  1. Keep this setup\n  2. Sign in again in this folder (same Apple Account)\n  3. Switch to another Apple Account\n  4. Quit\nChoose 1, 2, 3 or 4 (Enter keeps it): ",
            4,
            Some(1),
        )? {
            1 => break true,
            2 => {
                again = true;
                break false;
            }
            3 => {
                let old = dir.clone();
                c.say(&format!(
                    "A folder belongs to one Apple Account, so the other account gets a new folder. This folder stays as it is and stays signed in. To sign it out later, run: {}",
                    status::with_dir("skykeep-mcp logout", &old)
                ))?;
                dir = step_folder(c, &next_free(&old))?;
                c.dir = Some(dir.clone());
            }
            _ => return Ok(Outcome::Quit),
        }
    };
    if !keep {
        if !step_storage(c, env, &dir)? {
            return Ok(Outcome::Quit);
        }
        step_sign_in(c, env, &dir, &options.endpoints, again).await?;
        step_policy(c, &dir)?;
    }
    offer_permissions(c, &dir)?;
    c.say("\nStep 5 of 5: Check")?;
    let report = status::check(env, &dir);
    c.say(report.render().trim_end())?;
    let command = options.command.to_string_lossy();
    let folder = dir.to_string_lossy();
    let launch = format!("{} --state-dir {}", shell(&command), shell(&folder));
    let config = serde_json::json!({
        "command": command,
        "args": ["--state-dir", folder],
    });
    c.say(&format!(
        "\nMost MCP clients: add this line to the list of servers in the client's settings:\n\"icloud\": {config}\n\nHermes: add this to config.yaml (if it already has mcp_servers:, add only the icloud part under it):\nmcp_servers:\n  icloud:\n    command: {}\n    args:\n      - \"--state-dir\"\n      - {}\n    enabled: true\n    sampling:\n      enabled: false\n\nClaude Code and Codex: run one of these in a terminal:\nclaude mcp add --scope user icloud -- {launch}\ncodex mcp add icloud -- {launch}\n\nThe server's name in the client is now icloud. Rules the client keeps for the old name skykeep, such as tools it may call without asking, don't apply to icloud: set them again there.",
        yaml(&command),
        yaml(&folder),
    ))?;
    if !report.ready() {
        return Err("Some checks above need fixing. Follow the next step shown, then run skykeep-mcp status.");
    }
    let mode = survey(env, &dir)?.mode.unwrap_or_default();
    let mut done = String::from("\nSetup is complete.");
    if storage::effective_kind(&dir) == Ok(Kind::ProtectedFile) {
        done.push_str(" After each restart of this computer, run skykeep-mcp unlock once.");
    }
    match mode.as_str() {
        "reads_and_writes" | "writes_only" | "yolo" => done.push_str(&format!(" {CARD_NOTE}")),
        "none" => {
            let held = PrivateState::open(&dir)?;
            done.push(' ');
            done.push_str(status::no_mode_effect(&crate::setup(&held.path)?));
            done.push_str(&format!(" {CARD_NOTE}"));
        }
        _ => {}
    }
    c.say(&done)?;
    Ok(Outcome::Ready(dir))
}

/// Step 2. False when the person chose to quit.
fn step_storage(console: &mut dyn Console, env: &Env, dir: &Path) -> Result<bool, &'static str> {
    console.say("\nStep 2 of 5: Where your sign-in is kept\nYour Apple password is never stored. What is kept is the sign-in Apple hands back, so you don't have to type a code every time.")?;
    let held = PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let chosen = storage::kind(dir)?;
    let library = crate::setup(dir)?;
    let history = chosen.is_some()
        || library
            .state(quince_sync::queue::ACCOUNT_KEY)
            .map_err(|_| "library_unavailable")?
            .is_some()
        || dir.join("cookies.json").exists()
        || dir.join(storage::PROTECTED_FILE).exists();
    let current = chosen.or(history.then_some(Kind::SystemKeyring));
    let holder = env.holder.available();
    match current {
        Some(Kind::SystemKeyring) => {
            match storage::keyring_state(env, dir) {
                KeyringState::Ready => {
                    if chosen.is_none() {
                        storage::set_kind(dir, Kind::SystemKeyring)?;
                    }
                    console.say("Kept in your system keyring. Nothing to change.")?;
                    Ok(true)
                }
                KeyringState::Locked => Err(LOCKED_KEYRING),
                KeyringState::Unknown => Err(UNKNOWN_KEYRING),
                KeyringState::Missing => {
                    if !holder {
                        return Err(NO_SECURE_STORAGE);
                    }
                    console.say("Setup can't reach your system keyring, where this folder's sign-in was kept.")?;
                    if choose(
                        console,
                        "  1. Switch to a protected file\n  2. Quit\nChoose 1 or 2: ",
                        2,
                        None,
                    )? != 1
                        || !confirm_switch(console, Kind::SystemKeyring)?
                    {
                        return Ok(false);
                    }
                    // The sign-out stays owed to the keyring (its own note), and is finished, or
                    // demanded, before this folder ever uses the keyring again.
                    if let Ok(old) = (env.keyring)(dir) {
                        let _ = old.clear();
                    }
                    let _ = std::fs::remove_file(dir.join("cookies.json"));
                    console.say("Setup could not reach the system keyring, so an older sign-in may stay there under the service name \"skykeep-mcp\". Nothing of it is used from now on.")?;
                    storage::set_kind(dir, Kind::ProtectedFile)?;
                    new_protected(console, env, dir)?;
                    Ok(true)
                }
            }
        }
        Some(Kind::ProtectedFile) => match storage::protected_state(env, dir) {
            Ok(()) => {
                console.say("Kept in the protected file, which is unlocked. Nothing to change.")?;
                Ok(true)
            }
            Err(Closed::Missing) => {
                storage::set_kind(dir, Kind::ProtectedFile)?;
                new_protected(console, env, dir)?;
                Ok(true)
            }
            Err(Closed::Locked) | Err(Closed::WrongOrDamaged) => {
                console.say("The protected file is locked.")?;
                match choose(
                    console,
                    "  1. Unlock it with my passphrase\n  2. Start over with a new passphrase (you sign in to Apple again)\n  3. Quit\nChoose 1, 2 or 3: ",
                    3,
                    Some(1),
                )? {
                    1 => {
                        unlock_tries(console, env, dir, None)?;
                        Ok(true)
                    }
                    2 => {
                        if console.ask("This removes the protected file and the sign-in in it. Your cached data stays. Type start over to continue: ")?.trim() != "start over" {
                            return Ok(false);
                        }
                        storage::remove_protected(env, dir)?;
                        new_protected(console, env, dir)?;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            Err(Closed::HolderUnavailable) => {
                match storage::keyring_state(env, dir) {
                    KeyringState::Ready => {}
                    KeyringState::Locked => return Err(LOCKED_KEYRING),
                    KeyringState::Unknown => return Err(UNKNOWN_KEYRING),
                    KeyringState::Missing => return Err(NO_SECURE_STORAGE),
                }
                console.say("This computer blocks the kernel key storage the protected file needs, but your system keyring works.")?;
                if choose(
                    console,
                    "  1. Switch to the system keyring\n  2. Quit\nChoose 1 or 2: ",
                    2,
                    None,
                )? != 1
                    || !confirm_switch(console, Kind::ProtectedFile)?
                {
                    return Ok(false);
                }
                // Whatever the keyring still holds for this folder is older than the protected
                // file: erased first, and the switch waits until it is.
                let keyring = (env.keyring)(dir).map_err(|_| NO_SECURE_STORAGE)?;
                if keyring.clear().is_err() || keyring.sign_out_pending() {
                    return Err("Setup couldn't remove an older sign-in from the system keyring. Unlock the keyring, then run skykeep-mcp setup again.");
                }
                storage::remove_protected(env, dir)?;
                let _ = std::fs::remove_file(dir.join("cookies.json"));
                storage::set_kind(dir, Kind::SystemKeyring)?;
                console.say("Switched to the system keyring.")?;
                Ok(true)
            }
            Err(other) => Err(other.sentence()),
        },
        None => {
            let keyring = storage::keyring_state(env, dir);
            let kind = match (keyring, holder) {
                (KeyringState::Ready, true) => {
                    let n = choose(
                        console,
                        "  1. My computer's password store (the system keyring). Recommended.\n  2. A protected file in the iCloud-MCP folder, opened with a passphrase you choose. After each restart you run skykeep-mcp unlock once.\nChoose 1 or 2 (Enter picks 1): ",
                        2,
                        Some(1),
                    )?;
                    if n == 1 {
                        Kind::SystemKeyring
                    } else {
                        Kind::ProtectedFile
                    }
                }
                (KeyringState::Ready, false) => Kind::SystemKeyring,
                (KeyringState::Locked | KeyringState::Unknown, true) => {
                    console.say(if keyring == KeyringState::Locked {
                        "Your system keyring is locked, so setup can't keep the sign-in there right now."
                    } else {
                        "Setup can't tell whether your system keyring is locked, so it won't keep the sign-in there right now."
                    })?;
                    if choose(
                        console,
                        "  1. Use a protected file instead, opened with a passphrase you choose. After each restart you run skykeep-mcp unlock once.\n  2. Quit, so I can unlock the keyring first (log in to the desktop session) and run setup again\nChoose 1 or 2: ",
                        2,
                        None,
                    )? != 1
                    {
                        return Ok(false);
                    }
                    Kind::ProtectedFile
                }
                (KeyringState::Locked, false) => return Err(LOCKED_KEYRING),
                (KeyringState::Unknown, false) => return Err(UNKNOWN_KEYRING),
                (KeyringState::Missing, true) => {
                    console.say("This computer has no system keyring setup can use, so the sign-in goes in a protected file. You choose its passphrase, and after each restart you run skykeep-mcp unlock once.")?;
                    Kind::ProtectedFile
                }
                (KeyringState::Missing, false) => return Err(NO_SECURE_STORAGE),
            };
            storage::set_kind(dir, kind)?;
            match kind {
                Kind::SystemKeyring => {
                    console.say("Your sign-in will be kept in the system keyring.")?
                }
                Kind::ProtectedFile => new_protected(console, env, dir)?,
            }
            Ok(true)
        }
    }
}

fn confirm_switch(console: &mut dyn Console, old: Kind) -> Result<bool, &'static str> {
    let answer = console.ask(&format!(
        "Switching removes the sign-in kept in the {}, and you sign in to Apple again. Type switch to continue: ",
        old.describe()
    ))?;
    Ok(answer.trim() == "switch")
}

/// Why a new passphrase is refused, if it is.
pub fn weak_passphrase(passphrase: &str) -> Option<String> {
    if passphrase.chars().count() < storage::MIN_PASSPHRASE {
        return Some(format!(
            "That passphrase is shorter than {} characters.",
            storage::MIN_PASSPHRASE
        ));
    }
    let distinct: std::collections::BTreeSet<char> = passphrase.chars().collect();
    if distinct.len() < 5 {
        return Some(
            "That passphrase is too easy to guess. Use at least 5 different characters.".into(),
        );
    }
    None
}

/// Asks for a new passphrase twice and creates the protected file with it.
fn new_protected(console: &mut dyn Console, env: &Env, dir: &Path) -> Result<(), &'static str> {
    console.say(&format!(
        "Choose a passphrase for the protected file: at least {} characters. Nothing is shown as you type. iCloud-MCP can't recover it; if you lose it, run skykeep-mcp setup again and start over.",
        storage::MIN_PASSPHRASE
    ))?;
    for _ in 0..3 {
        let first = console.secret("New passphrase (hidden): ")?;
        if let Some(reason) = weak_passphrase(&first) {
            console.say(&reason)?;
            continue;
        }
        let second = console.secret("Type it again (hidden): ")?;
        if *first != *second {
            console.say("The two passphrases were different.")?;
            continue;
        }
        storage::create_protected(env, dir, &first).map_err(|c| c.sentence())?;
        console.say("Protected file created and unlocked.")?;
        return Ok(());
    }
    Err("No passphrase was set. Run skykeep-mcp setup again to choose one.")
}

/// Up to three passphrase tries. Caller holds the folder's lock.
fn unlock_tries(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    seconds: Option<u32>,
) -> Result<(), &'static str> {
    for _ in 0..3 {
        let passphrase = console.secret("Passphrase (hidden): ")?;
        match storage::unlock_with(env, dir, &passphrase, seconds) {
            Ok(()) => return Ok(()),
            Err(Closed::WrongOrDamaged) => console.say(Closed::WrongOrDamaged.sentence())?,
            Err(other) => return Err(other.sentence()),
        }
    }
    Err("Wrong passphrase or damaged file, three times. Run skykeep-mcp unlock again to try again.")
}

/// `skykeep-mcp unlock`. Every message names the folder in its commands.
pub fn unlock(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    hours: Option<u32>,
) -> Result<(), String> {
    let mut c = Scoped {
        inner: console,
        dir: Some(dir.to_path_buf()),
    };
    if !dir.exists() {
        return Err(c.scope(&format!(
            "No iCloud-MCP folder at {}. Run skykeep-mcp setup first.",
            dir.display()
        )));
    }
    let result = unlock_in(&mut c, env, dir, hours);
    result.map_err(|text| c.scope(text))
}

fn unlock_in(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    hours: Option<u32>,
) -> Result<(), &'static str> {
    if !dir.exists() {
        return Err("No iCloud-MCP folder there yet. Run skykeep-mcp setup first.");
    }
    let held = PrivateState::open(dir).map_err(|code| match code {
        "state_busy_retry" => {
            "The server is using the folder right now. Run skykeep-mcp unlock again in a moment."
        }
        other => plain(other),
    })?;
    let dir = held.path.as_path();
    match storage::kind(dir)? {
        None => return Err("Setup is not finished for this folder. Run skykeep-mcp setup."),
        Some(Kind::SystemKeyring) => return Err(
            "This folder keeps its sign-in in the system keyring, so there is nothing to unlock.",
        ),
        Some(Kind::ProtectedFile) => {}
    }
    match storage::protected_state(env, dir) {
        Ok(()) if hours.is_none() => {
            // A time limit from an earlier `unlock --hours` goes: plain unlock means until restart.
            storage::keep_unlocked(env, dir).map_err(|c| c.sentence())?;
            console.say("Already unlocked. It stays unlocked until this computer restarts.")?;
            return Ok(());
        }
        Ok(()) | Err(Closed::Locked) | Err(Closed::WrongOrDamaged) => {}
        Err(other) => return Err(other.sentence()),
    }
    unlock_tries(console, env, dir, hours.map(|h| h * 3600))?;
    console.say(&match hours {
        Some(h) => format!("Unlocked for {h} hours, or until this computer restarts."),
        None => "Unlocked until this computer restarts.".into(),
    })
}

/// Step 3. `again`: sign in even when a saved sign-in looks usable here (Apple refused it, or
/// the person asked). The account binding still refuses another account.
async fn step_sign_in(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    endpoints: &Endpoints,
    again: bool,
) -> Result<(), &'static str> {
    console.say("\nStep 3 of 5: Apple sign-in")?;
    let held = PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let store = storage::open_store_in(env, dir)?;
    let session = store.try_load().map_err(|_| "secure_storage_unavailable")?;
    let library = crate::setup(dir)?;
    if session.is_resumable() && !again {
        let binding = session
            .dsid
            .as_deref()
            .map(|dsid| crate::account::binding(&library, dsid))
            .transpose()?;
        match binding {
            Some(crate::account::Binding::Same) => {
                console.say(&format!(
                    "Already signed in as {}. Keeping it.",
                    session
                        .apple_id
                        .as_deref()
                        .map(status::mask)
                        .unwrap_or_default()
                ))?;
                return Ok(());
            }
            Some(crate::account::Binding::Other) => {
                return Err("account_mismatch_use_new_state_directory")
            }
            _ => {}
        }
    } else if session.apple_id.is_some() && !again {
        console.say("Your saved sign-in has expired. Sign in again.")?;
    }
    drop(library);
    console.say("Type your Apple Account (email or phone number), then its password. Neither is shown as you type. The password is used once and never stored.")?;
    let mut client = Client::with(dir, endpoints.clone(), store).map_err(login::auth_error)?;
    login::authenticate_with(&mut client, &mut |label, echo| match echo {
        Echo::Hidden => console.secret(label),
        Echo::Shown => console.ask(label).map(Zeroizing::new),
    })
    .await?;
    login::bind_account(dir, &mut client)?;
    console.say(&format!(
        "Signed in as {}.",
        client
            .session()
            .apple_id
            .as_deref()
            .map(status::mask)
            .unwrap_or_default()
    ))?;
    Ok(())
}

/// What every approval mode needs from the MCP client, as setup says it.
const CARD_NOTE: &str = "The client must be able to show iCloud-MCP's approval card; Hermes, Claude Code and Codex show it in their interactive apps. In a client without the card, or a session where nobody answers it, such as a scheduled job, actions that need approval are refused and nothing runs.";

/// Step 4. Runs after the sign-in: a policy saved before sign-in is bound to no account.
fn step_policy(console: &mut dyn Console, dir: &Path) -> Result<(), &'static str> {
    console.say(&format!("\nStep 4 of 5: Approval mode\nThis decides when the assistant has to ask you before it acts. Every choice asks on the approval card in your MCP client. {CARD_NOTE} You can change it later with an {}: line in permissions.yaml in this folder; a line there wins over what you choose here, and setup keeps it in step.", crate::permissions::MODE_KEY))?;
    let held = PrivateState::open(dir)?;
    let library = crate::setup(&held.path)?;
    if library
        .state(quince_sync::queue::ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .is_none()
    {
        return Err("login_required");
    }
    let existing = crate::policy(&library, &held.path);
    let in_file = file_mode(&library, &held.path);
    // The file's mode, kept with Enter in a folder that has no stored mode yet: it is stored
    // too, so queued changes are bound to it.
    let mut keep = None;
    match &existing {
        Ok(_) if in_file.is_some() => {
            let answer = console.ask(&format!(
                "Current mode: {} It is set in permissions.yaml ({}:), which replaces the mode chosen here; a new choice changes that line.\nPress Enter to keep it, or type change: ",
                status::describe_mode(in_file.as_deref().unwrap_or_default()),
                crate::permissions::MODE_KEY
            ))?;
            if answer.trim().is_empty() {
                if matches!(existing, Ok(Some(_))) {
                    return Ok(());
                }
                keep = MODES
                    .iter()
                    .map(|(m, _)| *m)
                    .find(|m| Some(*m) == in_file.as_deref());
            }
        }
        Ok(Some(_)) if crate::adopted(&library) => {
            let answer = console.ask(&format!(
                "Current mode: {}\nPress Enter to keep it, or type change: ",
                describe("none", &library)
            ))?;
            if answer.trim().is_empty() {
                return Ok(());
            }
        }
        Ok(Some((policy, _))) => {
            let answer = console.ask(&format!(
                "Current mode: {}\nPress Enter to keep it, or type change: ",
                status::describe_mode(&policy.mode)
            ))?;
            if answer.trim().is_empty() {
                return Ok(());
            }
        }
        Ok(None) if no_mode_chosen(&library) => {
            let answer = console.ask(&format!(
                "Current mode: {}\nPress Enter to keep it, or type change: ",
                describe("none", &library)
            ))?;
            if answer.trim().is_empty() {
                return Ok(());
            }
        }
        Err("policy_account_changed") | Err("policy_state_changed") => console.say(
            "The saved approval mode was set before sign-in, for another account or for another folder, so choose it again.",
        )?,
        _ => {}
    }
    // An approval mode can't be removed again, so the no-mode choice is offered only to a
    // folder that never had one.
    let raw = library
        .state("mcp.policy")
        .map_err(|_| "policy_unavailable")?;
    // A mode line in permissions.yaml would still apply, so no-mode is not offered then.
    let offer_none = raw.is_none() && in_file.is_none();
    let mut menu = format!(
        "  1. {} Recommended.\n  2. {}\n  3. {}\n",
        MODES[0].1, MODES[1].1, MODES[2].1
    );
    if offer_none {
        menu.push_str(&format!("  4. No approval mode (older behaviour). {} Once you pick 1, 2 or 3, this folder can't go back to 4. Most people should pick 1.\nChoose 1, 2, 3 or 4 (Enter picks 1): ", status::no_mode_choice(&library)));
    } else if in_file.is_some() {
        if keep.is_none() {
            console.say(&format!(
                "permissions.yaml sets {}:, so there is no choice without an approval mode.",
                crate::permissions::MODE_KEY
            ))?;
        }
        menu.push_str("Choose 1, 2 or 3 (Enter picks 1): ");
    } else {
        console.say("This folder has had an approval mode, so there is no choice without one.")?;
        menu.push_str("Choose 1, 2 or 3 (Enter picks 1): ");
    }
    let count = if offer_none { 4 } else { 3 };
    let mode = loop {
        if let Some(keep) = keep {
            break Some(keep);
        }
        let n = choose(console, &menu, count, Some(1))?;
        if n == 3
            && console
                .ask("The assistant could then read and change your data without asking. Deletions, moves to Recently Deleted and moves or renames of large Drive folders still ask on the card. Type confirm to allow that: ")?
                .trim()
                != "confirm"
        {
            continue;
        }
        if n == 4 {
            if console
                .ask("Without an approval mode the assistant's routine changes run without asking you. Type confirm to use no approval mode: ")?
                .trim()
                != "confirm"
            {
                continue;
            }
            break None;
        }
        break Some(MODES[n - 1].0);
    };
    let Some(mode) = mode else {
        let mut library = library;
        library
            .set_state(NO_MODE_KEY, "1")
            .map_err(|_| "policy_unavailable")?;
        let effect = status::no_mode_effect(&library);
        return console.say(&format!("Approval mode: none. {effect}"));
    };
    drop(library);
    drop(held);
    // The count the person agrees to is checked again under the lock that sets the changes
    // aside; anything queued meanwhile means asking again.
    loop {
        let ids: Vec<i64> = {
            let held = PrivateState::open(dir)?;
            let library = crate::setup(&held.path)?;
            // Kept from the file: an edit made while setup was asking wins, and nothing is saved.
            if keep.is_some() && file_mode(&library, &held.path).as_deref() != keep {
                return Err("permissions.yaml changed while setup was asking, so the approval mode was not saved. Run skykeep-mcp setup again to see the mode the file names now.");
            }
            library
                .outbox_all_pending()
                .map_err(|_| "library_unavailable")?
                .iter()
                .map(|p| p.id)
                .collect()
        };
        if !ids.is_empty()
            && console
                .ask(&format!(
                    "{} changes are waiting to be sent. Saving a mode sets them aside, and they will not be sent; skykeep-mcp quarantine lists them. Type confirm to continue: ",
                    ids.len()
                ))?
                .trim()
                != "confirm"
        {
            return Err("Approval mode not changed. Run skykeep-mcp setup again when you are ready.");
        }
        match crate::Server::open(dir)?.set_policy_if(mode, Some(&ids)) {
            Ok(saved) => {
                let held = PrivateState::open(dir)?;
                let _ = crate::setup(&held.path)?.set_state(NO_MODE_KEY, "");
                console.say(&format!(
                    "Approval mode saved: {}",
                    status::describe_mode(saved["mode"].as_str().unwrap_or_default())
                ))?;
                // A line in the file would win over the choice, so it is changed to match.
                let key = crate::permissions::MODE_KEY;
                // Kept from the file: the file already holds it and is never rewritten here.
                if keep.is_none() && crate::permissions::setting_in_file(&held.path, key).is_some()
                {
                    match crate::permissions::set_setting(&held.path, key, mode) {
                        Ok(()) => console.say(&format!("permissions.yaml now says {key}: {mode}."))?,
                        Err(reason) => console.say(&format!("permissions.yaml could not be changed to match ({reason}); its {key}: line still wins. Change it there."))?,
                    }
                }
                return Ok(());
            }
            Err("pending_changed") => {
                console.say("More changes were queued while you were answering.")?
            }
            Err(other) => return Err(other),
        }
    }
}

/// The starter label setup shows; tests answer it by this text.
pub const STARTER_CHOICE: &str =
    "  1. Write the starter file\n  2. Skip\nChoose 1 or 2 (Enter skips): ";

/// Offers a starter permissions file when the folder has none. Never replaces one.
fn offer_permissions(console: &mut dyn Console, dir: &Path) -> Result<(), &'static str> {
    let held = PrivateState::open(dir)?;
    let path = held.path.join(crate::permissions::FILE_NAME);
    if path.symlink_metadata().is_ok() {
        return Ok(());
    }
    let n = choose(
        console,
        &format!(
            "\nOptional: a permissions file says, for each tool and action, whether iCloud-MCP runs it, asks you first or refuses it. Setup can write a starter file to {}. In it, every deletion asks on the approval card in your MCP client, and it lists every name you can use. iCloud-MCP uses the file as soon as it exists, and you can change it later.\n{STARTER_CHOICE}",
            path.display()
        ),
        2,
        Some(2),
    )?;
    if n == 1 {
        crate::permissions::write_starter(&held.path)?;
        console.say("Starter permissions file written. To see the rules in use, run skykeep-mcp permissions show.")?;
    }
    Ok(())
}

#[cfg(test)]
mod shell_tests {
    #[test]
    fn a_path_with_spaces_or_quotes_stays_one_shell_word() {
        assert_eq!(
            super::shell("/opt/skykeep/skykeep-mcp"),
            "/opt/skykeep/skykeep-mcp"
        );
        assert_eq!(super::shell("/home/a b/x"), "'/home/a b/x'");
        assert_eq!(super::shell("/home/it's"), r"'/home/it'\''s'");
        assert_eq!(super::shell("~/x"), "'~/x'");
        assert_eq!(super::shell("/a;rm -rf x"), "'/a;rm -rf x'");
    }
}
