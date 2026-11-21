//! The guided setup, unlock and status, driven in-process with a scripted terminal, a fake
//! system keyring, a stand-in for the kernel key storage and loopback Apple fixtures. Every
//! value is fictional. These tests run one at a time: they share one storage environment.
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use quince_auth::{Endpoints, PlatformSecrets, Store};
use serde_json::{json, Value};
use skykeep_mcp::keyhold::{KeyHolder, MemoryHolder};
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

struct FakeKeyring(PathBuf);

impl FakeKeyring {
    fn check(&self) -> Result<(), String> {
        match fakes()
            .modes
            .get(&self.0)
            .copied()
            .unwrap_or(Keyring::Ready)
        {
            Keyring::Ready => Ok(()),
            Keyring::Locked => Err("Secret Service: object locked".into()),
            Keyring::Missing => Err("no Secret Service on the session bus".into()),
        }
    }
}

impl PlatformSecrets for FakeKeyring {
    fn get(&self, _: &str, name: &str) -> Result<Option<String>, String> {
        self.check()?;
        Ok(fakes().entries.get(&(self.0.clone(), name.into())).cloned())
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
}

/// The holder the environment uses, shared so tests can simulate a restart or a block.
struct Shared(&'static MemoryHolder);
impl KeyHolder for Shared {
    fn available(&self) -> bool {
        self.0.available()
    }
    fn get(&self, n: &str) -> Result<Option<Zeroizing<Vec<u8>>>, skykeep_mcp::keyhold::HoldError> {
        self.0.get(n)
    }
    fn put(
        &self,
        n: &str,
        k: &[u8],
        s: Option<u32>,
    ) -> Result<(), skykeep_mcp::keyhold::HoldError> {
        self.0.put(n, k, s)
    }
    fn forget(&self, n: &str) -> Result<(), skykeep_mcp::keyhold::HoldError> {
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
}
use Answer::{Ask, Cancel, Secret};

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
        match self.answers.pop_front() {
            Some(Ask(a)) => {
                self.screen.push_str(a);
                self.screen.push('\n');
                Ok(a.into())
            }
            Some(Cancel) => Err("input_cancelled"),
            Some(Secret(_)) => panic!("expected a hidden prompt at {label:?}\n{}", self.screen),
            None => panic!("unexpected prompt {label:?}\n{}", self.screen),
        }
    }
    fn secret(&mut self, label: &str) -> Result<Zeroizing<String>, &'static str> {
        self.screen.push_str(label);
        self.screen.push('\n');
        match self.answers.pop_front() {
            Some(Secret(a)) => Ok(Zeroizing::new(a.into())),
            Some(Cancel) => Err("input_cancelled"),
            Some(Ask(_)) => panic!("expected a shown prompt at {label:?}\n{}", self.screen),
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

fn sign_in_answers() -> Vec<Answer> {
    vec![Secret(ACCOUNT), Secret(PASSWORD), Ask("1"), Secret(CODE)]
}

async fn run(
    script: &mut Script,
    dir: &Path,
    server: &MockServer,
) -> Result<Outcome, &'static str> {
    setup::run(script, storage::env(), &options(dir, server)).await
}

/// The whole first run on a computer with a working keyring, with defaults everywhere.
async fn first_run_with_keyring(dir: &Path, server: &MockServer) -> Script {
    let mut answers = vec![Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
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

fn policy(dir: &Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    serde_json::from_str(&library.state("mcp.policy").unwrap().unwrap()).unwrap()
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
async fn first_run_signs_in_then_binds_the_strictest_mode_and_prints_the_client_line() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let script = first_run_with_keyring(&dir, &server).await;
    script.assert_no_secrets();
    let screen = &script.screen;
    for step in 1..=5 {
        assert!(screen.contains(&format!("Step {step} of 5")), "{screen}");
    }
    assert!(screen.contains("1. Ask me before every read and every change. Recommended."));
    assert!(
        screen.contains("Signed in as f***@e***.invalid"),
        "{screen}"
    );
    let line = format!(
        "\"skykeep\": {{\"args\":[\"--state-dir\",\"{}\"],\"command\":\"/opt/skykeep/skykeep-mcp\"}}",
        dir.display()
    );
    assert!(screen.contains(&line), "{screen}");
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
async fn a_rerun_asks_only_keep_switch_or_quit_and_keeps_everything() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    let revision = policy(&dir)["revision"].clone();
    let mut script = Script::new(vec![Ask(""), Ask("")]);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone()))
    );
    script.done();
    assert!(script
        .screen
        .contains("1. Keep this setup\n  2. Switch to another Apple Account\n  3. Quit"));
    assert!(!script.screen.contains("Step 3 of 5"), "no new sign-in");
    assert_eq!(
        policy(&dir)["revision"],
        revision,
        "the mode was not saved again"
    );

    let mut script = Script::new(vec![Ask(""), Ask("3")]);
    assert_eq!(run(&mut script, &dir, &server).await, Ok(Outcome::Quit));
    script.done();
}

#[tokio::test]
async fn switching_account_sets_up_a_new_folder_and_leaves_the_old_one_signed_in() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    first_run_with_keyring(&dir, &server).await;
    let mut answers = vec![Ask(""), Ask("2"), Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let mut script = Script::new(answers);
    let new = PathBuf::from(format!("{}-2", dir.display()));
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(new.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert!(skykeep_mcp::status::check(storage::env(), &dir).ready());
    assert!(skykeep_mcp::status::check(storage::env(), &new).ready());
}

#[tokio::test]
async fn an_interrupted_setup_continues_where_it_stopped() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let mut script = Script::new(vec![Ask(""), Ask(""), Secret(ACCOUNT), Cancel]);
    let stopped = run(&mut script, &dir, &server).await.unwrap_err();
    assert!(stopped.starts_with("Setup stopped."), "{stopped}");
    script.done();
    assert!(policy_missing(&dir));
    // Storage was chosen already: the next run goes on with the sign-in.
    let mut answers = vec![Ask("")];
    answers.extend(sign_in_answers());
    answers.push(Ask(""));
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert!(script
        .screen
        .contains("Kept in your system keyring. Nothing to change."));
}

fn policy_missing(dir: &Path) -> bool {
    quince_core::Library::open(dir)
        .unwrap()
        .state("mcp.policy")
        .unwrap()
        .is_none()
}

/// First run with no usable keyring: the protected file, its passphrase checks, and a restart.
#[tokio::test]
async fn without_a_keyring_the_protected_file_survives_a_restart_after_unlock() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    script.assert_no_secrets();
    assert!(script.screen.contains("shorter than 12 characters"));
    assert!(script
        .screen
        .contains("The two passphrases were different."));
    assert!(script.screen.contains("run skykeep-mcp unlock once"));
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::ProtectedFile));
    assert!(dir.join(storage::PROTECTED_FILE).exists());
    assert!(!dir.join("session.json").exists());
    assert!(
        fakes().entries.keys().all(|(d, _)| d != &dir),
        "nothing went to the keyring"
    );
    assert_folder_private_and_sealed(&dir);

    // A restart: the kernel forgets the key. The server says so and never guesses.
    holder().forget(&storage::key_name(&dir).unwrap()).unwrap();
    let status = tool(&dir, "auth_status").await;
    assert_eq!(status["status"], "locked");
    assert!(status["instructions"]
        .as_str()
        .unwrap()
        .starts_with(storage::LOCKED));
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
        report
            .render()
            .contains("Next step: Run skykeep-mcp unlock."),
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
    assert_eq!(
        tool(&dir, "auth_status").await["status"],
        "session_unverified"
    );
    assert!(skykeep_mcp::status::check(storage::env(), &dir).ready());

    // Unlocking again asks nothing.
    let mut script = Script::new(vec![]);
    assert_eq!(
        setup::unlock(&mut script, storage::env(), &dir, None),
        Ok(())
    );
    assert!(script.screen.contains("Already unlocked."));

    // Three wrong tries stop, with the same one answer each time.
    holder().forget(&storage::key_name(&dir).unwrap()).unwrap();
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
    script.done();
    assert!(storage::locked(&dir));

    // A re-run while locked offers the unlock in place.
    let mut script = Script::new(vec![Ask(""), Ask("1"), Secret(PASSPHRASE), Ask("")]);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
}

#[tokio::test]
async fn a_damaged_file_gives_the_same_answer_as_a_wrong_passphrase_and_never_opens() {
    let _one = serial().await;
    let (_parent, dir) = folder();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    assert!(storage::locked(&dir));
}

#[tokio::test]
async fn an_expired_session_signs_in_again_and_a_policy_set_before_sign_in_is_chosen_again() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A policy saved before any sign-in is bound to no account.
    skykeep_mcp::Server::open(&dir)
        .unwrap()
        .set_policy("yolo")
        .unwrap();
    assert!(policy(&dir)["account"].is_null());
    first_run_with_keyring(&dir, &server).await;
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
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
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
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert_eq!(storage::kind(&dir).unwrap(), Some(Kind::ProtectedFile));
    assert!(script.screen.contains("may stay there"));
    assert_folder_private_and_sealed(&dir);
}

#[tokio::test]
async fn switching_from_the_protected_file_removes_it() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
async fn with_no_secure_storage_at_all_setup_stops_and_writes_nothing() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    set_keyring(&dir, Keyring::Missing);
    holder().blocked.store(true, Ordering::SeqCst);
    let mut script = Script::new(vec![Ask("")]);
    let refused = run(&mut script, &dir, &server).await.unwrap_err();
    holder().blocked.store(false, Ordering::SeqCst);
    assert!(
        refused.starts_with("This computer has no usable secure storage"),
        "{refused}"
    );
    assert_eq!(storage::kind(&dir).unwrap(), None);
    assert!(!dir.join(storage::PROTECTED_FILE).exists());
}

#[tokio::test]
async fn never_asking_needs_a_typed_confirmation() {
    let _one = serial().await;
    let server = apple().await;
    let (_parent, dir) = folder();
    let mut answers = vec![Ask(""), Ask("")];
    answers.extend(sign_in_answers());
    answers.extend([Ask("3"), Ask("no"), Ask("3"), Ask("confirm")]);
    let mut script = Script::new(answers);
    assert_eq!(
        run(&mut script, &dir, &server).await,
        Ok(Outcome::Ready(dir.clone())),
        "{}",
        script.screen
    );
    script.done();
    assert_eq!(policy(&dir)["mode"], "yolo");
}

#[test]
fn status_masks_the_account() {
    assert_eq!(
        skykeep_mcp::status::mask("fixture@example.invalid"),
        "f***@e***.invalid"
    );
    assert_eq!(skykeep_mcp::status::mask("+00 555 0123"), "***23");
}
