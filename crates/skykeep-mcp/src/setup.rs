//! `skykeep-mcp setup` and `skykeep-mcp unlock`: the guided, re-runnable first-time flow.
//!
//! Every answer comes from the person's own terminal through [`Console`]. Passwords, codes and
//! passphrases only ever go through [`Console::secret`]; nothing typed there is shown, kept in
//! a file, or put in any message. Each step is saved as soon as it is done, so a setup that
//! stops part way picks up where it left off.
use crate::login::{self, Echo};
use crate::state::PrivateState;
use crate::status::{self, MODES};
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

pub struct Options {
    /// The folder given with `--state-dir`, if any.
    pub state_dir: Option<PathBuf>,
    /// The folder suggested when none was given.
    pub default_dir: Option<PathBuf>,
    pub endpoints: Endpoints,
    /// This program, for the client configuration line.
    pub command: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready(PathBuf),
    Quit,
}

const NO_SECURE_STORAGE: &str = "This computer has no usable secure storage: the system keyring is not reachable and the kernel key storage is blocked, so setup can't keep you signed in. Start a system keyring, or run skykeep-mcp on a computer that allows the kernel key storage.";

/// `~/.local/share/skykeep-mcp`.
pub fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local/share/skykeep-mcp"))
}

/// A sentence for each error code the steps below can stop with. Codes never carry secrets.
pub fn plain(code: &'static str) -> &'static str {
    match code {
        "credentials_rejected" => "Apple didn't accept that Apple Account or password. Run skykeep-mcp setup again to retry.",
        "code_incorrect_or_expired" => "The verification code was wrong or expired three times. Run skykeep-mcp setup again to get a new code.",
        "auth_cooldown_try_later" => "Apple asked to wait before the next sign-in attempt. Try again later.",
        "account_locked_visit_account_website" => "Apple has locked this Apple Account. Unlock it at iforgot.apple.com, then run setup again.",
        "secure_storage_unavailable" => "The sign-in could not be saved in the chosen storage. Run skykeep-mcp status to see why.",
        "regional_login_retry_required" => "Apple asked to sign in again. Run skykeep-mcp setup again.",
        "account_mismatch_use_new_state_directory" => "This folder belongs to a different Apple Account. Run setup again and choose a new folder for this account.",
        "security_key_sign_in_unsupported" => "This account signs in with a security key, which skykeep-mcp can't use yet.",
        "too_many_code_requests_try_later" => "Setup already asked Apple for several codes. Wait a while, then run setup again.",
        "input_cancelled" => "Setup stopped. Run skykeep-mcp setup again to continue where you left off.",
        "input_timeout" => "Setup waited three minutes without an answer and stopped. Run skykeep-mcp setup again to continue.",
        "state_busy_retry" => "The server is using the folder right now. Run setup again in a moment.",
        "state_must_be_owned_and_private" => "That folder is not private to you. Choose another folder, or make it yours and run chmod 700 on it.",
        "unsafe_state_contents" => "A file in that folder can be read by others, or is a link. Choose another folder.",
        "unsafe_state_path" => "That folder path goes through a link or '..'. Use the folder's real path.",
        "state_unavailable" | "library_unavailable" => "Setup can't open that folder. Check that its parent folder exists and belongs to you.",
        "login_incomplete" | "authentication_unavailable" | "login_required" => "Sign-in didn't finish. Run skykeep-mcp setup again.",
        "interactive_terminal_required" | "terminal_unavailable" => "Setup lost the terminal. Run it again in a terminal window.",
        other => other,
    }
}

fn choose(
    console: &mut dyn Console,
    label: &str,
    count: usize,
    default: usize,
) -> Result<usize, &'static str> {
    let mut retry = false;
    loop {
        let answer = console.ask(&if retry {
            format!("Please type a number from 1 to {count}. {label}")
        } else {
            label.to_string()
        })?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=count).contains(&n) => return Ok(n),
            _ => retry = true,
        }
    }
}

fn expand(answer: &str) -> Result<PathBuf, &'static str> {
    let answer = answer.trim();
    let path = if let Some(rest) = answer.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").ok_or("state_unavailable")?).join(rest)
    } else {
        PathBuf::from(answer)
    };
    std::path::absolute(path).map_err(|_| "state_unavailable")
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
    console.say("\nStep 1 of 5: Folder\nSkykeep keeps its private data in one folder: its cache, its settings and the choice of where your sign-in is kept.")?;
    let answer = console.ask(&format!(
        "Folder: {}\nPress Enter to use it, or type another folder: ",
        suggested.display()
    ))?;
    let path = if answer.trim().is_empty() {
        suggested.to_path_buf()
    } else {
        expand(&answer)?
    };
    let dir = prepare(&path)?;
    console.say(&format!("Using {}.", dir.display()))?;
    Ok(dir)
}

/// What is already done in a folder.
struct Found {
    storage_ready: bool,
    kind: Kind,
    account: Option<String>,
    policy: Option<String>,
}

impl Found {
    fn complete(&self) -> bool {
        self.storage_ready && self.account.is_some() && self.policy.is_some()
    }
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
    let policy = match crate::policy(&library, dir) {
        Ok(Some((p, _))) => Some(p.mode),
        _ => None,
    };
    Ok(Found {
        storage_ready,
        kind,
        account,
        policy,
    })
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

/// The whole guided flow.
pub async fn run(
    console: &mut dyn Console,
    env: &Env,
    options: &Options,
) -> Result<Outcome, &'static str> {
    console.say("Skykeep setup: five short steps. You can stop at any time with Ctrl-C and run setup again later; it continues where it stopped.")?;
    let suggested = options
        .state_dir
        .clone()
        .or_else(|| options.default_dir.clone())
        .ok_or("Setup can't find your home folder. Run it again with --state-dir FOLDER.")?;
    let mut dir = step_folder(console, &suggested).map_err(plain)?;
    let keep = loop {
        let found = survey(env, &dir).map_err(plain)?;
        if !found.complete() {
            break false;
        }
        console.say(&format!(
            "\nThis folder is already set up. Sign-in kept in the {}, signed in as {}, approval mode: {}",
            found.kind.describe(),
            found.account.as_deref().unwrap_or_default(),
            status::describe_mode(found.policy.as_deref().unwrap_or_default()),
        ))?;
        match choose(
            console,
            "  1. Keep this setup\n  2. Switch to another Apple Account\n  3. Quit\nChoose 1, 2 or 3 (Enter keeps it): ",
            3,
            1,
        )
        .map_err(plain)?
        {
            1 => break true,
            2 => {
                console.say("A folder belongs to one Apple Account, so the other account gets a new folder. This folder stays as it is and stays signed in; to sign it out later, run skykeep-mcp logout --state-dir with this folder.")?;
                dir = step_folder(console, &next_free(&dir)).map_err(plain)?;
            }
            _ => return Ok(Outcome::Quit),
        }
    };
    if !keep {
        if !step_storage(console, env, &dir).map_err(plain)? {
            return Ok(Outcome::Quit);
        }
        step_sign_in(console, env, &dir, &options.endpoints)
            .await
            .map_err(plain)?;
        step_policy(console, &dir).map_err(plain)?;
    }
    console.say("\nStep 5 of 5: Check")?;
    let report = status::check(env, &dir);
    console.say(report.render().trim_end())?;
    let config = serde_json::json!({
        "command": options.command.to_string_lossy(),
        "args": ["--state-dir", dir.to_string_lossy()],
    });
    console.say(&format!(
        "\nAdd this line to the list of servers in your MCP client's settings:\n\"skykeep\": {config}"
    ))?;
    if !report.ready() {
        return Err("Some checks above need fixing. Follow the next step shown, then run skykeep-mcp status.");
    }
    console.say(&format!(
        "\nSetup is complete.{}",
        if storage::effective_kind(&dir) == Ok(Kind::ProtectedFile) {
            " After each restart of this computer, run skykeep-mcp unlock once."
        } else {
            ""
        }
    ))?;
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
        Some(Kind::SystemKeyring) => match storage::keyring_state(env, dir) {
            KeyringState::Ready => {
                if chosen.is_none() {
                    storage::set_kind(dir, Kind::SystemKeyring)?;
                }
                console.say("Kept in your system keyring. Nothing to change.")?;
                Ok(true)
            }
            KeyringState::Locked => Err("Your system keyring is locked. Unlock it (log in to your desktop), then run skykeep-mcp setup again."),
            KeyringState::Missing => {
                if !holder {
                    return Err(NO_SECURE_STORAGE);
                }
                console.say("Setup can't reach your system keyring, where this folder's sign-in was kept.")?;
                if choose(
                    console,
                    "  1. Switch to a protected file\n  2. Quit\nChoose 1 or 2: ",
                    2,
                    2,
                )? != 1
                {
                    return Ok(false);
                }
                if !confirm_switch(console, Kind::SystemKeyring)? {
                    return Ok(false);
                }
                if let Ok(old) = (env.keyring)(dir) {
                    let _ = old.clear();
                }
                let _ = std::fs::remove_file(dir.join("cookies.json"));
                console.say("Setup could not reach the system keyring, so an older sign-in may stay there under the name skykeep-mcp. Nothing of it is used from now on.")?;
                storage::set_kind(dir, Kind::ProtectedFile)?;
                new_protected(console, env, dir)?;
                Ok(true)
            }
        },
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
                    1,
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
                if storage::keyring_state(env, dir) != KeyringState::Ready {
                    return Err(NO_SECURE_STORAGE);
                }
                console.say("This computer blocks the kernel key storage the protected file needs, but your system keyring works.")?;
                if choose(
                    console,
                    "  1. Switch to the system keyring\n  2. Quit\nChoose 1 or 2: ",
                    2,
                    2,
                )? != 1
                    || !confirm_switch(console, Kind::ProtectedFile)?
                {
                    return Ok(false);
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
                        "  1. My computer's password store (the system keyring). Recommended.\n  2. A protected file in the Skykeep folder, opened with a passphrase you choose. After each restart you run skykeep-mcp unlock once.\nChoose 1 or 2 (Enter picks 1): ",
                        2,
                        1,
                    )?;
                    if n == 1 {
                        Kind::SystemKeyring
                    } else {
                        Kind::ProtectedFile
                    }
                }
                (KeyringState::Ready, false) => Kind::SystemKeyring,
                (KeyringState::Locked, true) => {
                    console.say("Your system keyring is locked.")?;
                    if choose(
                        console,
                        "  1. Use a protected file instead, opened with a passphrase you choose. After each restart you run skykeep-mcp unlock once.\n  2. Quit, so I can unlock the keyring and run setup again\nChoose 1 or 2: ",
                        2,
                        2,
                    )? != 1
                    {
                        return Ok(false);
                    }
                    Kind::ProtectedFile
                }
                (KeyringState::Missing, true) => {
                    console.say("This computer has no system keyring setup can use, so the sign-in goes in a protected file. You choose its passphrase, and after each restart you run skykeep-mcp unlock once.")?;
                    Kind::ProtectedFile
                }
                (_, false) => return Err(NO_SECURE_STORAGE),
            };
            storage::set_kind(dir, kind)?;
            match kind {
                Kind::SystemKeyring => console.say("Your sign-in will be kept in the system keyring.")?,
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

/// Asks for a new passphrase twice and creates the protected file with it.
fn new_protected(console: &mut dyn Console, env: &Env, dir: &Path) -> Result<(), &'static str> {
    console.say(&format!(
        "Choose a passphrase for the protected file: at least {} characters. Nothing is shown as you type. Skykeep can't recover it; if you lose it, run setup again and start over.",
        storage::MIN_PASSPHRASE
    ))?;
    for _ in 0..3 {
        let first = console.secret("New passphrase (hidden): ")?;
        if first.chars().count() < storage::MIN_PASSPHRASE {
            console.say(&format!(
                "That passphrase is shorter than {} characters.",
                storage::MIN_PASSPHRASE
            ))?;
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
    Err("Wrong passphrase or damaged file, three times. Run the command again to try again.")
}

/// `skykeep-mcp unlock`.
pub fn unlock(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    hours: Option<u32>,
) -> Result<(), &'static str> {
    if !dir.exists() {
        return Err("No Skykeep folder there yet. Run skykeep-mcp setup first.");
    }
    let held = PrivateState::open(dir).map_err(plain)?;
    let dir = held.path.as_path();
    if storage::effective_kind(dir)? != Kind::ProtectedFile {
        return Err(
            "This folder keeps its sign-in in the system keyring, so there is nothing to unlock.",
        );
    }
    if hours.is_none() && storage::protected_state(env, dir).is_ok() {
        console.say("Already unlocked.")?;
        return Ok(());
    }
    unlock_tries(console, env, dir, hours.map(|h| h * 3600)).map_err(plain)?;
    console.say(&match hours {
        Some(h) => format!("Unlocked for {h} hours, or until this computer restarts."),
        None => "Unlocked until this computer restarts.".into(),
    })
}

/// Step 3.
async fn step_sign_in(
    console: &mut dyn Console,
    env: &Env,
    dir: &Path,
    endpoints: &Endpoints,
) -> Result<(), &'static str> {
    console.say("\nStep 3 of 5: Apple sign-in")?;
    let held = PrivateState::open(dir)?;
    let dir = held.path.as_path();
    let store = storage::open_store_in(env, dir)?;
    let session = store.try_load().map_err(|_| "secure_storage_unavailable")?;
    let library = crate::setup(dir)?;
    if session.is_resumable() {
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
    } else if session.apple_id.is_some() {
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

/// Step 4. Signs in first: a policy saved before sign-in is bound to no account.
fn step_policy(console: &mut dyn Console, dir: &Path) -> Result<(), &'static str> {
    console.say("\nStep 4 of 5: Approval mode\nThis decides when the assistant has to ask you before it acts. Approval happens in your MCP client, never by typing a password.")?;
    let held = PrivateState::open(dir)?;
    let library = crate::setup(&held.path)?;
    if library
        .state(quince_sync::queue::ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .is_none()
    {
        return Err("login_required");
    }
    match crate::policy(&library, &held.path) {
        Ok(Some((policy, _))) => {
            let answer = console.ask(&format!(
                "Current mode: {}\nPress Enter to keep it, or type change: ",
                status::describe_mode(&policy.mode)
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
    let menu = format!(
        "  1. {} Recommended.\n  2. {}\n  3. {}\nChoose 1, 2 or 3 (Enter picks 1): ",
        MODES[0].1, MODES[1].1, MODES[2].1
    );
    let mode = loop {
        let n = choose(console, &menu, 3, 1)?;
        if n == 3
            && console
                .ask("The assistant could then change and delete notes and reminders without asking. Type confirm to allow that: ")?
                .trim()
                != "confirm"
        {
            continue;
        }
        break MODES[n - 1].0;
    };
    let pending = library
        .outbox_all_pending()
        .map_err(|_| "library_unavailable")?
        .len();
    drop(library);
    drop(held);
    if pending > 0
        && console
            .ask(&format!(
                "{pending} changes are waiting to be sent. Saving a mode sets them aside, and they will not be sent; skykeep-mcp quarantine lists them. Type confirm to continue: "
            ))?
            .trim()
            != "confirm"
    {
        return Err("Approval mode not changed. Run skykeep-mcp setup again when you are ready.");
    }
    let saved = crate::Server::open(dir)?.set_policy(mode)?;
    console.say(&format!(
        "Approval mode saved: {}",
        status::describe_mode(saved["mode"].as_str().unwrap_or_default())
    ))
}
