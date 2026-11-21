//! The guided setup, unlock and status, driven in-process with a scripted terminal, a fake
//! system keyring, a stand-in for the kernel key storage and loopback Apple fixtures. Every
//! value is fictional. These tests run one at a time: they share one storage environment.
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use quince_auth::{Endpoints, PlatformSecrets, Store};
use serde_json::{json, Value};
use skykeep_mcp::keyhold::{HoldError, KeyHolder, MemoryHolder};
use skykeep_mcp::setup::{self, Console, Options, Outcome};
use skykeep_mcp::storage::{self, Env, Kind};
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, OnceLock};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

const ACCOUNT: &str = "fixture@example.invalid";
const PASSWORD: &str = "SYNTHETIC-PASSWORD-SENTINEL";
const CODE: &str = "424242";
const PASSPHRASE: &str = "SYNTHETIC-PASSPHRASE-SENTINEL";
const TRUST: &str = "SYNTHETIC-TRUST-SENTINEL";
const SECRETS: [&str; 5] = [ACCOUNT, PASSWORD, CODE, PASSPHRASE, TRUST];

/// What the fake keyring answers for a folder.
#[derive(Clone, Copy, PartialEq)]
enum Keyring {
    Ready,
    Locked,
    Missing,
    /// Entries answer, but the lock state can't be read.
    Unknown,
}

#[derive(Default)]
struct Fakes {
    modes: HashMap<PathBuf, Keyring>,
    entries: HashMap<(PathBuf, String), String>,
}

fn fakes() -> MutexGuard<'static, Fakes> {
    static FAKES: OnceLock<Mutex<Fakes>> = OnceLock::new();
    FAKES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A Secret Service as it answers: an entry that isn't there reads as nothing even in a locked
/// collection; one that is there is refused while locked; writes need it unlocked.
struct FakeKeyring(PathBuf);

impl FakeKeyring {
    fn mode(&self) -> Keyring {
        fakes()
            .modes
            .get(&self.0)
            .copied()
            .unwrap_or(Keyring::Ready)
    }
    fn check(&self) -> Result<(), String> {
        match self.mode() {
            Keyring::Ready | Keyring::Unknown => Ok(()),
            Keyring::Locked => Err("Secret Service: object locked".into()),
            Keyring::Missing => Err("no Secret Service on the session bus".into()),
        }
    }
}

impl PlatformSecrets for FakeKeyring {
    fn get(&self, _: &str, name: &str) -> Result<Option<String>, String> {
        let found = fakes().entries.get(&(self.0.clone(), name.into())).cloned();
        if self.mode() == Keyring::Locked && found.is_none() {
            return Ok(None);
        }
        self.check()?;
        Ok(found)
    }
    fn set(&self, _: &str, name: &str, value: &str) -> Result<(), String> {
        self.check()?;
        fakes()
            .entries
            .insert((self.0.clone(), name.into()), value.into());
        Ok(())
    }
    fn delete(&self, _: &str, name: &str) -> Result<(), String> {
        self.check()?;
        fakes().entries.remove(&(self.0.clone(), name.into()));
        Ok(())
    }
    fn locked(&self, _: &str) -> Result<bool, String> {
        match self.mode() {
            Keyring::Ready => Ok(false),
            Keyring::Locked => Ok(true),
            Keyring::Missing | Keyring::Unknown => {
                Err("no Secret Service on the session bus".into())
            }
        }
    }
}

fn keyring_entries(dir: &Path) -> usize {
    fakes().entries.keys().filter(|(d, _)| d == dir).count()
}

/// The holder the environment uses, shared so tests can simulate a restart or a block.
struct Shared(&'static MemoryHolder);
impl KeyHolder for Shared {
    fn available(&self) -> bool {
        self.0.available()
    }
    fn get(&self, n: &str) -> Result<Option<Zeroizing<Vec<u8>>>, HoldError> {
        self.0.get(n)
    }
    fn put(&self, n: &str, k: &[u8], s: Option<u32>) -> Result<(), HoldError> {
        self.0.put(n, k, s)
    }
    fn forget(&self, n: &str) -> Result<(), HoldError> {
        self.0.forget(n)
    }
}

fn holder() -> &'static MemoryHolder {
    static HOLDER: OnceLock<MemoryHolder> = OnceLock::new();
    HOLDER.get_or_init(MemoryHolder::default)
}

/// One test at a time, with the shared environment installed and reset.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = ONE.lock().await;
    // As the program itself runs: new files private to this user.
    unsafe {
        libc::umask(0o077);
    }
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        storage::use_env(Env {
            keyring: Box::new(|dir: &Path| {
                let dir = dir.canonicalize()?;
                Store::with_secrets(&dir, Box::new(FakeKeyring(dir.clone())))
            }),
            holder: Box::new(Shared(holder())),
        })
        .unwrap();
    });
    holder().blocked.store(false, Ordering::SeqCst);
    guard
}

fn set_keyring(dir: &Path, mode: Keyring) {
    fakes().modes.insert(dir.canonicalize().unwrap(), mode);
}

enum Answer {
    Ask(&'static str),
    Secret(&'static str),
    Cancel,
    /// A shown answer, typed after something else happened meanwhile.
    AskAfter(&'static str, Box<dyn FnOnce() + Send>),
}
use Answer::{Ask, AskAfter, Cancel, Secret};

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[derive(Default)]
struct Script {
    answers: VecDeque<Answer>,
    /// Everything the person would see on screen: messages and prompts, never hidden answers.
    screen: String,
}

impl Script {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            answers: answers.into(),
            screen: String::new(),
        }
    }
    fn done(&self) {
        assert!(
            self.answers.is_empty(),
            "unused answers; screen:\n{}",
            self.screen
        );
    }
    fn assert_no_secrets(&self) {
        for secret in SECRETS {
            assert!(
                !self.screen.contains(secret),
                "screen shows {secret}:\n{}",
                self.screen
            );
        }
    }
}

impl Console for Script {
    fn say(&mut self, text: &str) -> Result<(), &'static str> {
        self.screen.push_str(text);
        self.screen.push('\n');
        Ok(())
    }
    fn ask(&mut self, label: &str) -> Result<String, &'static str> {
        self.screen.push_str(label);
        let answer = match self.answers.pop_front() {
            Some(Ask(a)) => a,
            Some(AskAfter(a, meanwhile)) => {
                meanwhile();
                a
            }
            Some(Cancel) => return Err("input_cancelled"),
            Some(Secret(_)) => panic!("expected a hidden prompt at {label:?}\n{}", self.screen),
            None => panic!("unexpected prompt {label:?}\n{}", self.screen),
        };
        self.screen.push_str(answer);
        self.screen.push('\n');
        Ok(answer.into())
    }
    fn secret(&mut self, label: &str) -> Result<Zeroizing<String>, &'static str> {
        self.screen.push_str(label);
        self.screen.push('\n');
        match self.answers.pop_front() {
            Some(Secret(a)) => Ok(Zeroizing::new(a.into())),
            Some(Cancel) => Err("input_cancelled"),
            Some(Ask(_)) | Some(AskAfter(..)) => {
                panic!("expected a shown prompt at {label:?}\n{}", self.screen)
            }
            None => panic!("unexpected prompt {label:?}\n{}", self.screen),
        }
    }
}

async fn apple() -> MockServer {
    let server = MockServer::start().await;
    for (verb, endpoint, response) in [
        ("POST", "/appleauth/auth/signin/init", ResponseTemplate::new(200).set_body_json(json!({"salt":B64.encode([0x77;16]),"b":B64.encode([0x5e;256]),"c":"challenge","iteration":10,"protocol":"s2k"}))),
        ("POST", "/appleauth/auth/signin/complete", ResponseTemplate::new(409).insert_header("X-Apple-Session-Token","SYNTHETIC-PENDING-TOKEN").set_body_json(json!({}))),
        ("PUT", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth", ResponseTemplate::new(200).set_body_json(json!({"trustedDeviceCount":1,"trustedPhoneNumbers":[]}))),
        ("POST", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth/2sv/trust", ResponseTemplate::new(204).insert_header("X-Apple-TwoSV-Trust-Token",TRUST).insert_header("X-Apple-Session-Token","SYNTHETIC-COMPLETE-TOKEN")),
        ("POST", "/setup/ws/1/accountLogin", ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{}}))),
    ] { Mock::given(method(verb)).and(path(endpoint)).respond_with(response).mount(&server).await; }
    server
}

/// Apple refuses the saved sign-in once: the check before the menu sees a dead session.
async fn reject_saved_sign_in(server: &MockServer) {
    for endpoint in ["/setup/ws/1/validate", "/setup/ws/1/accountLogin"] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(421))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
    }
}

fn options(dir: &Path, server: &MockServer) -> Options {
    Options {
        state_dir: Some(dir.to_path_buf()),
        default_dir: None,
        endpoints: Endpoints::at(&server.uri()),
        command: PathBuf::from("/opt/skykeep/skykeep-mcp"),
    }
}

/// A parent folder private to this user, and the Skykeep folder inside it (not made yet).
fn folder() -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = parent.path().canonicalize().unwrap().join("skykeep");
    (parent, dir)
}

fn made(dir: &Path) {
    std::fs::create_dir(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn sign_in_answers() -> Vec<Answer> {
    vec![Secret(ACCOUNT), Secret(PASSWORD), Ask("1"), Secret(CODE)]
}

/// The folder prompt, and the "Create it?" prompt when the folder isn't there yet.
fn folder_answers(dir: &Path) -> Vec<Answer> {
    if dir.exists() {
        vec![Ask("")]
    } else {
        vec![Ask(""), Ask("")]
    }
}

async fn run(script: &mut Script, dir: &Path, server: &MockServer) -> Result<Outcome, String> {
    setup::run(script, storage::env(), &options(dir, server)).await
}

async fn ready(answers: Vec<Answer>, dir: &Path, server: &MockServer) -> Script {
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, dir, server).await,
        Ok(Outcome::Ready(dir.to_path_buf())),
        "{}",
        script.screen
    );
    script.done();
    script
}

/// The whole first run on a computer with a working keyring, with defaults everywhere.
async fn first_run_with_keyring(dir: &Path, server: &MockServer) -> Script {
    let mut answers = folder_answers(dir);
    answers.push(Ask(""));
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    ready(answers, dir, server).await
}

fn policy(dir: &Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    serde_json::from_str(&library.state("mcp.policy").unwrap().unwrap()).unwrap()
}

fn policy_missing(dir: &Path) -> bool {
    quince_core::Library::open(dir)
        .unwrap()
        .state("mcp.policy")
        .unwrap()
        .is_none()
}

/// No file in the folder holds a secret in the clear, and every file and the folder are private.
fn assert_folder_private_and_sealed(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        if meta.is_file() {
            assert_eq!(meta.mode() & 0o077, 0, "{:?} is shared", entry.path());
            let bytes = std::fs::read(entry.path()).unwrap();
            for secret in [
                PASSWORD,
                CODE,
                PASSPHRASE,
                TRUST,
                "SYNTHETIC-COMPLETE-TOKEN",
            ] {
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "{secret} in {:?}",
                    entry.path()
                );
            }
        }
    }
}

async fn tool(dir: &Path, name: &str) -> Value {
    let mut server = skykeep_mcp::Server::open(dir).unwrap();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let r = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":{}}}))
        .await
        .unwrap();
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn first_run_signs_in_then_binds_the_strictest_mode_and_prints_both_client_forms() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let script = first_run_with_keyring(&dir, &server).await;
    script.assert_no_secrets();
    let screen = &script.screen;
    for step in 1..=5 {
        assert!(screen.contains(&format!("Step {step} of 5")), "{screen}");
    }
    assert!(screen.contains(&format!("Create {}? [Y/n]", dir.display())));
    assert!(screen.contains("1. Ask me before every read and every change. Recommended."));
    assert!(screen.contains("Hermes can't yet"), "{screen}");
    assert!(
        screen.contains("Signed in as f***@e***.invalid"),
        "{screen}"
    );
    let line = format!(
        "\"skykeep\": {{\"args\":[\"--state-dir\",\"{}\"],\"command\":\"/opt/skykeep/skykeep-mcp\"}}",
        dir.display()
    );
    assert!(screen.contains(&line), "{screen}");
    let hermes = format!(
        "mcp_servers:\n  skykeep:\n    command: \"/opt/skykeep/skykeep-mcp\"\n    args:\n      - \"--state-dir\"\n      - \"{}\"\n    enabled: true\n    sampling:\n      enabled: false",
        dir.display()
    );
    assert!(screen.contains(&hermes), "{screen}");
    assert!(screen.contains("choose 4 there"), "{screen}");
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::SystemKeyring));
    let saved = policy(&dir);
    assert_eq!(saved["mode"], "reads_and_writes");
    assert!(
        saved["account"].is_string(),
        "bound to the signed-in account"
    );
    assert!(skykeep_mcp::status::check(storage::env(), &dir).ready());
    assert_folder_private_and_sealed(&dir);
}

#[tokio::test]
async fn a_rerun_checks_the_sign_in_and_asks_keep_sign_in_again_switch_or_quit() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    let revision = policy(&dir)["revision"].clone();
    let script = ready(vec![Ask(""), Ask("")], &dir, &server).await;
    assert!(script
        .screen
        .contains("Checking the saved sign-in with Apple"));
    assert!(script.screen.contains("1. Keep this setup\n  2. Sign in again in this folder (same Apple Account)\n  3. Switch to another Apple Account\n  4. Quit"));
    assert!(!script.screen.contains("Step 3 of 5"), "no new sign-in");
    assert_eq!(
        policy(&dir)["revision"],
        revision,
        "the mode was not saved again"
    );

    let mut script = Script::new(vec![Ask(""), Ask("4")]);
    assert_eq!(run(&mut script, &dir, &server).await, Ok(Outcome::Quit));
    script.done();
}

/// Apple refused the saved sign-in: setup says so and signs in again in the same folder.
#[tokio::test]
async fn a_sign_in_apple_rejected_is_renewed_in_the_same_folder() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    reject_saved_sign_in(&server).await;
    let mut answers = vec![Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    assert!(
        script
            .screen
            .contains("Apple no longer accepts the sign-in saved in this folder"),
        "{}",
        script.screen
    );
    assert!(!script.screen.contains("1. Keep this setup"));
    assert!(!script.screen.contains("Already signed in"));
}

#[tokio::test]
async fn signing_in_again_on_request_keeps_the_account_and_refuses_another() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    let mut answers = vec![Ask(""), Ask("2")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    assert!(script.screen.contains("Type your Apple Account"));

    let mut script = Script::new(vec![
        Ask(""),
        Ask("2"),
        Secret("someone-else@example.invalid"),
    ]);
    let refused = run(&mut script, &dir, &server).await.unwrap_err();
    assert!(
        refused.starts_with("This folder belongs to a different Apple Account"),
        "{refused}"
    );
    script.done();
}

#[tokio::test]
async fn switching_account_sets_up_a_new_folder_and_leaves_the_old_one_signed_in() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    let new = PathBuf::from(format!("{}-2", dir.display()));
    let mut answers = vec![Ask(""), Ask("3"), Ask(""), Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(new.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert!(
        script.screen.contains(&format!(
            "To sign it out later, run: skykeep-mcp logout --state-dir {}",
            dir.display()
        )),
        "{}",
        script.screen
    );
    assert!(skykeep_mcp::status::check(storage::env(), &dir).ready());
    assert!(skykeep_mcp::status::check(storage::env(), &new).ready());
}

#[tokio::test]
async fn an_interrupted_setup_continues_where_it_stopped() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let mut script = Script::new(vec![Ask(""), Ask(""), Ask(""), Secret(ACCOUNT), Cancel]);
    let stopped = run(&mut script, &dir, &server).await.unwrap_err();
    assert_eq!(
        stopped,
        format!(
            "Setup stopped. Run skykeep-mcp setup --state-dir {} again to continue where you left off.",
            dir.display()
        )
    );
    script.done();
    assert!(policy_missing(&dir));
    // Storage was chosen already: the next run goes on with the sign-in.
    let mut answers = vec![Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    assert!(script
        .screen
        .contains("Kept in your system keyring. Nothing to change."));
}

#[tokio::test]
async fn the_folder_prompt_takes_quotes_refuses_relative_paths_and_asks_before_creating() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let quoted = leak(format!("  \"{}\"  ", dir.display()));
    let mut script = Script::new(vec![
        Ask("relative/folder"),
        Ask(quoted),
        Ask("n"),
        Ask(quoted),
        Ask("y"),
        Cancel,
    ]);
    assert!(run(&mut script, &dir, &server).await.is_err());
    script.done();
    assert!(
        script
            .screen
            .contains("Type the full path, starting with / or ~. \"relative/folder\" would be a folder inside"),
        "{}",
        script.screen
    );
    assert!(!PathBuf::from("relative").exists());
    assert!(dir.is_dir(), "made after the yes");
    assert_eq!(
        script
            .screen
            .matches(&format!("Create {}? [Y/n]", dir.display()))
            .count(),
        2
    );
}

/// First run with no usable keyring: the protected file, its passphrase checks, and a restart.
#[tokio::test]
async fn without_a_keyring_the_protected_file_survives_a_restart_after_unlock() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    set_keyring(&dir, Keyring::Missing);
    let mut answers = vec![
        Ask(""),
        Secret("too-short"),
        Secret(PASSPHRASE),
        Secret("SYNTHETIC-DIFFERENT-PASSPHRASE"),
        Secret(PASSPHRASE),
        Secret(PASSPHRASE),
    ];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    script.assert_no_secrets();
    assert!(script.screen.contains("shorter than 12 characters"));
    assert!(script
        .screen
        .contains("The two passphrases were different."));
    assert!(
        script.screen.contains(&format!(
            "run skykeep-mcp unlock --state-dir {} once",
            dir.display()
        )),
        "{}",
        script.screen
    );
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::ProtectedFile));
    assert!(dir.join(storage::PROTECTED_FILE).exists());
    assert!(!dir.join("session.json").exists());
    assert_eq!(keyring_entries(&dir), 0, "nothing went to the keyring");
    assert_folder_private_and_sealed(&dir);

    // A restart: the kernel forgets the key. The server says so and never guesses.
    holder().forget(&storage::key_name(&dir).unwrap()).unwrap();
    let status = tool(&dir, "auth_status").await;
    assert_eq!(status["status"], "locked");
    assert!(status["instructions"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "locked: run skykeep-mcp unlock --state-dir {} in a terminal",
            dir.display()
        )));
    // Reads without a consent form, so the tool reaches the storage.
    skykeep_mcp::Server::open(&dir)
        .unwrap()
        .set_policy("writes_only")
        .unwrap();
    let sync = tool(&dir, "sync").await;
    assert_eq!(sync["error"], storage::LOCKED);
    let report = skykeep_mcp::status::check(storage::env(), &dir);
    assert!(!report.ready());
    assert!(
        report.render().contains(&format!(
            "Next step: Run skykeep-mcp unlock --state-dir {}.",
            dir.display()
        )),
        "{}",
        report.render()
    );

    // A wrong passphrase, then the right one.
    let mut script = Script::new(vec![
        Secret("SYNTHETIC-WRONG-PASSPHRASE"),
        Secret(PASSPHRASE),
    ]);
    assert_eq!(
        setup::unlock(&mut script, storage::env(), &dir, None),
        Ok(())
    );
    script.done();
    assert!(script.screen.contains("Wrong passphrase or damaged file."));
    assert!(script
        .screen
        .contains("Unlocked until this computer restarts."));
    script.assert_no_secrets();
    let name = storage::key_name(&dir).unwrap();
    assert_eq!(holder().timeouts.lock().unwrap()[&name], None);
    assert_eq!(
        tool(&dir, "auth_status").await["status"],
        "session_unverified"
    );
    assert!(skykeep_mcp::status::check(storage::env(), &dir).ready());

    // Unlocking again asks nothing; with --hours it asks once and sets the timeout.
    let mut script = Script::new(vec![]);
    assert_eq!(
        setup::unlock(&mut script, storage::env(), &dir, None),
        Ok(())
    );
    assert!(script.screen.contains("Already unlocked."));
    let mut script = Script::new(vec![Secret(PASSPHRASE)]);
    assert_eq!(
        setup::unlock(&mut script, storage::env(), &dir, Some(2)),
        Ok(())
    );
    assert_eq!(holder().timeouts.lock().unwrap()[&name], Some(7200));
    assert!(script.screen.contains("Unlocked for 2 hours"));
    // A plain unlock afterwards removes that time limit without asking anything.
    let mut script = Script::new(vec![]);
    assert_eq!(
        setup::unlock(&mut script, storage::env(), &dir, None),
        Ok(())
    );
    assert!(script.screen.contains("Already unlocked"));
    assert_eq!(holder().timeouts.lock().unwrap()[&name], None);

    // Three wrong tries stop, with the same one answer each time.
    holder().forget(&name).unwrap();
    let mut script = Script::new(vec![
        Secret("SYNTHETIC-WRONG-1"),
        Secret("SYNTHETIC-WRONG-2"),
        Secret("SYNTHETIC-WRONG-3"),
    ]);
    let refused = setup::unlock(&mut script, storage::env(), &dir, Some(2)).unwrap_err();
    assert!(
        refused.starts_with("Wrong passphrase or damaged file"),
        "{refused}"
    );
    assert!(refused.contains(&format!("skykeep-mcp unlock --state-dir {}", dir.display())));
    script.done();
    assert_eq!(
        storage::protected_problem(&dir),
        Some(storage::Closed::Locked)
    );

    // A re-run while locked offers the unlock in place.
    ready(
        vec![Ask(""), Ask("1"), Secret(PASSPHRASE), Ask("")],
        &dir,
        &server,
    )
    .await;
}

#[tokio::test]
async fn a_damaged_file_gives_the_same_answer_as_a_wrong_passphrase_and_never_opens() {
    let _one = serial().await;
    let (_parent, dir) = folder();
    made(&dir);
    {
        let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
        storage::set_kind(&held.path, Kind::ProtectedFile).unwrap();
        storage::create_protected(storage::env(), &held.path, PASSPHRASE).unwrap();
    }
    let file = dir.join(storage::PROTECTED_FILE);
    let mut bytes = std::fs::read(&file).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&file, &bytes).unwrap();
    assert_eq!(
        storage::open_store(&dir).err(),
        Some(storage::LOCKED),
        "the held key no longer opens it"
    );
    holder().forget(&storage::key_name(&dir).unwrap()).unwrap();
    let mut script = Script::new(vec![
        Secret(PASSPHRASE),
        Secret(PASSPHRASE),
        Secret(PASSPHRASE),
    ]);
    assert!(setup::unlock(&mut script, storage::env(), &dir, None).is_err());
    assert_eq!(
        script
            .screen
            .matches("Wrong passphrase or damaged file.")
            .count(),
        3
    );
    // A cut header is damage the server can name without any passphrase.
    std::fs::write(&file, b"SKPF").unwrap();
    let status = tool(&dir, "auth_status").await;
    assert_eq!(status["status"], "storage_problem");
    assert!(status["instructions"]
        .as_str()
        .unwrap()
        .starts_with("protected file damaged"));
}

#[tokio::test]
async fn unlock_says_what_is_missing_before_asking_for_a_passphrase() {
    let _one = serial().await;
    let (_parent, dir) = folder();
    made(&dir);
    let mut script = Script::new(vec![]);
    let refused = setup::unlock(&mut script, storage::env(), &dir, None).unwrap_err();
    assert_eq!(
        refused,
        format!(
            "Setup is not finished for this folder. Run skykeep-mcp setup --state-dir {}.",
            dir.display()
        )
    );
    {
        let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
        storage::set_kind(&held.path, Kind::ProtectedFile).unwrap();
    }
    let refused = setup::unlock(&mut script, storage::env(), &dir, None).unwrap_err();
    assert!(refused.starts_with("No protected file yet"), "{refused}");
    let status = tool(&dir, "auth_status").await;
    assert_eq!(status["status"], "storage_problem");
    assert!(status["instructions"]
        .as_str()
        .unwrap()
        .starts_with("sign-in storage not ready"));
    holder().blocked.store(true, Ordering::SeqCst);
    let status = tool(&dir, "auth_status").await;
    holder().blocked.store(false, Ordering::SeqCst);
    assert!(status["instructions"]
        .as_str()
        .unwrap()
        .contains("kernel key storage"));
    script.done();
}

#[tokio::test]
async fn an_expired_session_signs_in_again_and_a_policy_set_before_sign_in_is_chosen_again() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    // A policy saved before any sign-in is bound to no account.
    skykeep_mcp::Server::open(&dir)
        .unwrap()
        .set_policy("yolo")
        .unwrap();
    assert!(policy(&dir)["account"].is_null());
    let script = first_run_with_keyring(&dir, &server).await;
    assert!(
        !script.screen.contains("  4. No approval mode"),
        "a folder that had a mode is not offered none"
    );
    let saved = policy(&dir);
    assert_eq!(saved["mode"], "reads_and_writes");
    assert!(saved["account"].is_string());

    // The saved sign-in lapses: tokens gone, the account kept.
    {
        let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
        let store = storage::open_store(&held.path).unwrap();
        let mut session = store.try_load().unwrap();
        session.session_token = None;
        session.trust_token = None;
        store.save(&session).unwrap();
    }
    let report = skykeep_mcp::status::check(storage::env(), &dir);
    assert!(report.render().contains("The saved sign-in has expired."));
    let mut answers = vec![Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    assert!(script.screen.contains("Your saved sign-in has expired."));
    assert!(script
        .screen
        .contains("Current mode: Ask me before every read"));
}

#[tokio::test]
async fn a_folder_of_another_account_refuses_the_sign_in_and_keeps_nothing() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    quince_core::Library::open(&dir)
        .unwrap()
        .set_state("account", "synthetic-other-account-tag")
        .unwrap();
    let mut answers = vec![Ask("")];
    answers.extend(sign_in_answers());
    let mut script = Script::new(answers);
    let refused = run(&mut script, &dir, &server).await.unwrap_err();
    assert!(
        refused.starts_with("This folder belongs to a different Apple Account"),
        "{refused}"
    );
    script.done();
    let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
    assert!(!storage::open_store(&held.path)
        .unwrap()
        .try_load()
        .unwrap()
        .is_resumable());
}

/// A locked collection with no entry yet reads as empty: setup must still see the lock, and
/// say so before asking for any Apple credentials.
#[tokio::test]
async fn a_locked_keyring_on_a_first_run_is_named_before_any_credential_is_asked() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    set_keyring(&dir, Keyring::Locked);
    assert_eq!(
        storage::keyring_state(storage::env(), &dir),
        storage::KeyringState::Locked
    );
    let mut script = Script::new(vec![Ask(""), Ask("2")]);
    assert_eq!(run(&mut script, &dir, &server).await, Ok(Outcome::Quit));
    script.done();
    assert!(script.screen.contains("Your system keyring is locked"));
    assert!(script.screen.contains("unlock the keyring first"));
    assert!(
        !script.screen.contains("Apple Account"),
        "{}",
        script.screen
    );

    let mut answers = vec![Ask(""), Ask("1"), Secret(PASSPHRASE), Secret(PASSPHRASE)];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    ready(answers, &dir, &server).await;
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::ProtectedFile));
}

#[tokio::test]
async fn a_locked_keyring_is_never_switched_away_from_and_a_missing_one_switches_after_confirmation(
) {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    set_keyring(&dir, Keyring::Locked);
    let mut script = Script::new(vec![Ask("")]);
    let refused = run(&mut script, &dir, &server).await.unwrap_err();
    assert!(
        refused.starts_with("Your system keyring is locked."),
        "{refused}"
    );
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::SystemKeyring));

    set_keyring(&dir, Keyring::Missing);
    // Declining the confirmation changes nothing.
    let mut script = Script::new(vec![Ask(""), Ask("1"), Ask("no")]);
    assert_eq!(run(&mut script, &dir, &server).await, Ok(Outcome::Quit));
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::SystemKeyring));

    let mut answers = vec![
        Ask(""),
        Ask("1"),
        Ask("switch"),
        Secret(PASSPHRASE),
        Secret(PASSPHRASE),
    ];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let script = ready(answers, &dir, &server).await;
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::ProtectedFile));
    assert!(script.screen.contains("may stay there"));
    assert_folder_private_and_sealed(&dir);
}

/// The reviewers' reproduction: a sign-out owed to an unreachable keyring must not come back
/// as "Already signed in" when the folder later switches back to that keyring.
#[tokio::test]
async fn switching_back_to_the_keyring_never_resumes_the_session_left_there() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    set_keyring(&dir, Keyring::Missing);
    let mut answers = vec![
        Ask(""),
        Ask("1"),
        Ask("switch"),
        Secret(PASSPHRASE),
        Secret(PASSPHRASE),
    ];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    ready(answers, &dir, &server).await;
    assert!(
        keyring_entries(&dir) > 0,
        "the old entries were out of reach"
    );

    set_keyring(&dir, Keyring::Ready);
    holder().blocked.store(true, Ordering::SeqCst);
    let mut answers = vec![Ask(""), Ask("1"), Ask("switch")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let mut script = Script::new(answers);
    let outcome = run(&mut script, &dir, &server).await;
    holder().blocked.store(false, Ordering::SeqCst);
    assert_eq!(
        outcome,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert!(!script.screen.contains("Already signed in"));
    assert!(script.screen.contains("Type your Apple Account"));
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::SystemKeyring));
    assert!(!dir.join(storage::PROTECTED_FILE).exists());
}

#[tokio::test]
async fn switching_from_the_protected_file_removes_it() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    {
        let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
        storage::set_kind(&held.path, Kind::ProtectedFile).unwrap();
        storage::create_protected(storage::env(), &held.path, PASSPHRASE).unwrap();
    }
    holder().blocked.store(true, Ordering::SeqCst);
    let mut answers = vec![Ask(""), Ask("1"), Ask("switch")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let mut script = Script::new(answers);
    let outcome = run(&mut script, &dir, &server).await;
    holder().blocked.store(false, Ordering::SeqCst);
    assert_eq!(
        outcome,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::SystemKeyring));
    assert!(!dir.join(storage::PROTECTED_FILE).exists());
}

#[tokio::test]
async fn with_no_secure_storage_at_all_setup_stops_in_plain_words_and_writes_nothing() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    set_keyring(&dir, Keyring::Missing);
    holder().blocked.store(true, Ordering::SeqCst);
    let mut script = Script::new(vec![Ask("")]);
    let refused = run(&mut script, &dir, &server).await.unwrap_err();
    let report = skykeep_mcp::status::check(storage::env(), &dir).render();
    holder().blocked.store(false, Ordering::SeqCst);
    assert!(
        refused.starts_with("Setup can't find a safe place on this computer to keep your sign-in. If you are inside Docker or a similar container, run Skykeep directly on the computer."),
        "{refused}"
    );
    assert!(!report.contains("protected file instead"), "{report}");
    assert!(report.contains("Docker"), "{report}");
    assert_eq!(storage::kind(&dir).unwrap(), None);
    assert!(!dir.join(storage::PROTECTED_FILE).exists());
}

#[tokio::test]
async fn never_asking_needs_a_typed_confirmation() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let mut answers = folder_answers(&dir);
    answers.push(Ask(""));
    answers.extend(sign_in_answers());
    answers.extend([Ask("3"), Ask("no"), Ask("3"), Ask("confirm")]);
    ready(answers, &dir, &server).await;
    assert_eq!(policy(&dir)["mode"], "yolo");
}

/// For a client without approval forms: no policy at all, the old behaviour, chosen on purpose.
#[tokio::test]
async fn no_approval_mode_leaves_the_folder_without_a_policy_and_counts_as_done() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let mut answers = folder_answers(&dir);
    answers.push(Ask(""));
    answers.extend(sign_in_answers());
    answers.extend([Ask("4"), Ask("no"), Ask("4"), Ask("confirm")]);
    let script = ready(answers, &dir, &server).await;
    assert!(script.screen.contains("  4. No approval mode, for MCP clients that can't show approval forms (such as Hermes today)."));
    assert!(script
        .screen
        .contains("deletions wait until you approve them with skykeep-mcp approve --state-dir"));
    assert!(policy_missing(&dir));
    let report = skykeep_mcp::status::check(storage::env(), &dir);
    assert!(report.ready(), "{}", report.render());
    assert!(report.render().contains("Approval mode: none"));
    let script = ready(vec![Ask(""), Ask("")], &dir, &server).await;
    assert!(script.screen.contains("approval mode: none"));
}

/// The count the person agreed to is checked again under the lock that sets changes aside.
#[tokio::test]
async fn changes_queued_during_the_confirmation_are_asked_about_again() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    quince_core::Library::open(&dir)
        .unwrap()
        .outbox_push("notes", "synthetic-note-1", "save", "{}")
        .unwrap();
    let queued = dir.clone();
    let mut answers = vec![Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    answers.push(AskAfter(
        "confirm",
        Box::new(move || {
            quince_core::Library::open(&queued)
                .unwrap()
                .outbox_push("notes", "synthetic-note-2", "save", "{}")
                .unwrap();
        }),
    ));
    answers.push(Ask("confirm"));
    let script = ready(answers, &dir, &server).await;
    assert!(script.screen.contains("1 changes are waiting"));
    assert!(script
        .screen
        .contains("More changes were queued while you were answering."));
    assert!(script.screen.contains("2 changes are waiting"));
    assert_eq!(policy(&dir)["mode"], "reads_and_writes");
}

#[test]
fn set_policy_refuses_when_the_waiting_writes_are_not_the_ones_agreed_to() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = skykeep_mcp::Server::open(dir.path()).unwrap();
    let id = quince_core::Library::open(dir.path())
        .unwrap()
        .outbox_push("notes", "synthetic-note", "save", "{}")
        .unwrap();
    assert_eq!(
        server.set_policy_if("writes_only", Some(&[])).err(),
        Some("pending_changed")
    );
    assert!(server.set_policy_if("writes_only", Some(&[id])).is_ok());
}

#[test]
fn status_masks_the_account_and_names_the_folder_in_commands() {
    assert_eq!(
        skykeep_mcp::status::mask("fixture@example.invalid"),
        "f***@e***.invalid"
    );
    assert_eq!(skykeep_mcp::status::mask("+00 555 0123"), "***23");
    let odd = Path::new("/srv/my folder/it's");
    assert_eq!(
        skykeep_mcp::status::with_dir("Run skykeep-mcp unlock, then skykeep-mcp status.", odd),
        "Run skykeep-mcp unlock --state-dir '/srv/my folder/it'\\''s', then skykeep-mcp status --state-dir '/srv/my folder/it'\\''s'."
    );
    assert_eq!(
        skykeep_mcp::status::with_dir("skykeep-mcp setup --state-dir NEW_FOLDER", odd),
        "skykeep-mcp setup --state-dir NEW_FOLDER"
    );
    if let Some(default) = setup::default_dir() {
        assert_eq!(
            skykeep_mcp::status::with_dir("Run skykeep-mcp unlock.", &default),
            "Run skykeep-mcp unlock."
        );
    }
}

#[test]
fn weak_passphrases_are_refused_in_plain_words() {
    assert!(setup::weak_passphrase("short").is_some());
    assert!(setup::weak_passphrase("aaaaaaaaaaaaaaaa").is_some());
    assert!(setup::weak_passphrase("abababababababab").is_some());
    assert!(setup::weak_passphrase("abcdabcdabcdabcd").is_some());
    assert!(setup::weak_passphrase("correct horse battery").is_none());
}

/// Only setup, status and unlock know the default folder; every other command needs the flag.
#[test]
fn the_default_folder_flag_is_left_out_only_for_commands_that_know_the_default() {
    let Some(default) = setup::default_dir() else {
        return;
    };
    let with = |command: &str| {
        skykeep_mcp::status::with_dir(&format!("Run skykeep-mcp {command}."), &default)
    };
    for command in ["setup", "status", "unlock"] {
        assert_eq!(with(command), format!("Run skykeep-mcp {command}."));
    }
    for command in ["approve", "logout", "login", "quarantine"] {
        assert!(
            with(command).starts_with(&format!("Run skykeep-mcp {command} --state-dir /")),
            "{}",
            with(command)
        );
    }
}

/// A folder that ran require-approval earlier keeps it under "no approval mode": setup and
/// status say every change waits for approval, not that routine changes run without asking.
#[tokio::test]
async fn no_approval_mode_tells_the_truth_about_an_earlier_require_approval() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    quince_core::Library::open(&dir)
        .unwrap()
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    let mut answers = vec![Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.extend([Ask("4"), Ask("no"), Ask("4"), Ask("confirm")]);
    let script = ready(answers, &dir, &server).await;
    assert!(
        !script.screen.contains("Routine changes run without asking"),
        "{}",
        script.screen
    );
    assert!(script
        .screen
        .contains("Every change waits until you approve it"));
    let report = skykeep_mcp::status::check(storage::env(), &dir).render();
    assert!(
        report.contains("Every change waits until you approve it"),
        "{report}"
    );
    assert_eq!(
        quince_core::Library::open(&dir)
            .unwrap()
            .state("mcp.require_write_approval")
            .unwrap()
            .as_deref(),
        Some("true"),
        "setup never loosens the older setting"
    );
}

#[tokio::test]
async fn an_unknown_lock_state_is_reported_as_unknown_and_a_found_entry_needs_no_lock_question() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    made(&dir);
    set_keyring(&dir, Keyring::Unknown);
    assert_eq!(
        storage::keyring_state(storage::env(), &dir),
        storage::KeyringState::Unknown
    );
    let report = skykeep_mcp::status::check(storage::env(), &dir).render();
    assert!(
        report.contains("can't tell whether it is locked"),
        "{report}"
    );
    // Setup asks for nothing from Apple while the lock state is unknown.
    let mut script = Script::new(vec![Ask(""), Ask("2")]);
    assert_eq!(run(&mut script, &dir, &server).await, Ok(Outcome::Quit));
    assert!(
        !script.screen.contains("Apple Account"),
        "{}",
        script.screen
    );
    // An entry that reads back means the keyring answered: ready, no lock question needed.
    let canonical = dir.canonicalize().unwrap();
    {
        let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
        let store = storage::open_store(&held.path).unwrap();
        fakes().modes.insert(canonical.clone(), Keyring::Ready);
        store
            .save(&quince_auth::session::Session {
                session_token: Some("SYNTHETIC-TOKEN".into()),
                ..Default::default()
            })
            .unwrap();
        fakes().modes.insert(canonical.clone(), Keyring::Unknown);
    }
    assert_eq!(
        storage::keyring_state(storage::env(), &dir),
        storage::KeyringState::Ready
    );
}

#[tokio::test]
async fn the_home_folder_itself_is_refused_with_the_suggested_folder_named() {
    let _one = serial().await;
    let server = apple().await;
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return;
    };
    if !Path::new(&home).is_dir() {
        return;
    }
    let (_parent, dir) = folder();
    let home_typed = leak(PathBuf::from(&home).display().to_string());
    let mut script = Script::new(vec![Ask("~"), Ask(home_typed), Cancel]);
    assert!(run(&mut script, &dir, &server).await.is_err());
    script.done();
    assert_eq!(
        script.screen.matches("not your home folder").count(),
        2,
        "{}",
        script.screen
    );
    assert!(script.screen.contains("~/.local/share/skykeep-mcp"));
    assert!(!script.screen.contains("chmod 700"));
}

#[tokio::test]
async fn unlock_names_a_missing_folder() {
    let _one = serial().await;
    let (_parent, dir) = folder();
    let mut script = Script::new(vec![]);
    let refused = setup::unlock(&mut script, storage::env(), &dir, None).unwrap_err();
    assert!(
        refused.starts_with(&format!("No Skykeep folder at {}.", dir.display())),
        "{refused}"
    );
}
