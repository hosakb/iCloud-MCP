//! The approval mode and the exchange folder as settings in permissions.yaml: what the file
//! accepts, that it wins over the older stored values, that an edit counts at the next call,
//! that a broken file never loosens, that looser changes are reported, the folder checks, and
//! the terminal shortcuts that edit the file. Synthetic data only, no network.
use serde_json::{json, Value};
use skykeep_mcp::permissions;
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Folder {
    dir: tempfile::TempDir,
    server: Server,
    library: quince_core::Library,
}

impl Folder {
    fn path(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
}

fn private(prefix: &str) -> tempfile::TempDir {
    // A plain name: a folder whose name starts with a dot is refused as an exchange folder.
    let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A private folder bound to a synthetic account with one reminder list and one reminder, in
/// `mode`, after an initialize that offers approval cards.
async fn folder(mode: Option<&str>) -> Folder {
    let dir = private("state");
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminders(&[quince_core::reminders::Reminder {
            guid: "R-1".into(),
            list_guid: "list".into(),
            title: "Synthetic reminder".into(),
            ..Default::default()
        }])
        .unwrap();
    if let Some(mode) = mode {
        retried(|| server.set_policy(mode));
    }
    server
        .handle(json!({"id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}}))
        .await;
    Folder {
        dir,
        server,
        library,
    }
}

fn write_file(dir: &Path, text: &str) {
    let path = dir.join(permissions::FILE_NAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn call(id: i64, name: &str, args: Value) -> Value {
    json!({"id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

/// `f` again while it is `state_busy_retry`, for the calls that take the folder lock outside
/// a tool call (see [`handle`]).
fn retried<T>(mut f: impl FnMut() -> Result<T, &'static str>) -> T {
    for _ in 0..500 {
        match f() {
            Err("state_busy_retry") => std::thread::sleep(std::time::Duration::from_millis(10)),
            other => return other.unwrap(),
        }
    }
    panic!("the private folder stayed busy");
}

/// The reply to a tool call, asked again while it is `state_busy_retry`, as a client does.
/// A test here that starts a terminal command forks this process, and the child holds every
/// open folder lock for a moment, until it runs the command. The lock itself is unchanged.
async fn handle(server: &mut Server, request: Value) -> Value {
    for _ in 0..500 {
        let reply = server.handle(request.clone()).await.unwrap();
        let busy = reply["result"]["isError"] == true
            && reply["result"]["content"][0]["text"]
                .as_str()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
                .is_some_and(|t| t["error"] == "state_busy_retry");
        if !busy {
            return reply;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the private folder stayed busy: {request}");
}

/// Whether the call stopped at an approval card; the card is closed again.
async fn asks(f: &mut Folder, id: i64, name: &str, args: Value) -> bool {
    let reply = handle(&mut f.server, call(id, name, args)).await;
    if reply["method"] == "elicitation/create" {
        f.server
            .handle(json!({"jsonrpc":"2.0","id":reply["id"],"result":{"action":"cancel"}}))
            .await;
        return true;
    }
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    false
}

#[test]
fn the_file_takes_an_approval_mode_and_an_exchange_folder_and_nothing_secret() {
    let parsed = permissions::parse_settings(
        "version: 1\napproval_mode: yolo   # comment\nexchange_folder: ~/Skykeep-Exchange\nrules:\n  notes_list: deny\n",
    )
    .unwrap();
    assert_eq!(parsed.mode.as_deref(), Some("yolo"));
    assert_eq!(parsed.exchange.as_deref(), Some("~/Skykeep-Exchange"));
    assert_eq!(parsed.rules.len(), 1);
    let quoted = permissions::parse_settings("exchange_folder: \"/srv/a b\"\n").unwrap();
    assert_eq!(quoted.exchange.as_deref(), Some("/srv/a b"));
    let none = permissions::parse_settings("version: 1\n").unwrap();
    assert_eq!((none.mode, none.exchange), (None, None));
    for (text, says) in [
        (
            "approval_mode: everything\n",
            "reads_and_writes, writes_only or yolo",
        ),
        ("approval_mode:\n", "reads_and_writes, writes_only or yolo"),
        ("approval_mode: yolo\napproval_mode: yolo\n", "listed twice"),
        ("exchange_folder: Exchange\n", "full path"),
        ("exchange_folder: ../x\n", "full path"),
        ("exchange_folder: /srv/../etc\n", ". or .."),
        ("exchange_folder: /srv/a\u{202E}b\n", "hidden"),
        ("exchange_folder:\n", "full path"),
        ("password: hunter2\n", "never go in this file"),
        ("apple_id_password: x\n", "never go in this file"),
        ("passphrase: x\n", "never go in this file"),
        ("verification_code: 123456\n", "never go in this file"),
    ] {
        let error = permissions::parse_settings(text).unwrap_err();
        assert!(error.contains(says), "{text:?}: {error}");
        assert!(
            !error.contains("hunter2"),
            "a secret is never repeated: {error}"
        );
    }
}

#[tokio::test]
async fn the_approval_mode_in_the_file_wins_and_counts_at_the_next_call() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    assert!(!asks(&mut f, 1, "reminders_lists", json!({})).await);
    write_file(&dir, "version: 1\napproval_mode: reads_and_writes\n");
    assert!(asks(&mut f, 2, "reminders_lists", json!({})).await);
    write_file(&dir, "version: 1\napproval_mode: yolo\n");
    assert!(!asks(&mut f, 3, "reminders_lists", json!({})).await);
    assert!(
        !asks(
            &mut f,
            4,
            "reminders_write",
            json!({"action":"create","list_guid":"list","title":"Synthetic created"})
        )
        .await
    );
    // Removing the line gives the mode chosen in setup back.
    write_file(&dir, "version: 1\n");
    assert!(
        asks(
            &mut f,
            5,
            "reminders_write",
            json!({"action":"create","list_guid":"list","title":"Synthetic again"})
        )
        .await
    );
}

#[tokio::test]
async fn deletions_still_need_a_person_whatever_mode_the_file_sets() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    write_file(
        &dir,
        "version: 1\napproval_mode: yolo\nrules:\n  reminders_write.delete: allow\n",
    );
    assert!(
        asks(
            &mut f,
            1,
            "reminders_write",
            json!({"action":"delete","guid":"R-1"})
        )
        .await
    );
    // A folder without a mode from setup follows the file's mode the same way.
    let mut g = folder(None).await;
    let dir = g.path();
    write_file(&dir, "version: 1\napproval_mode: yolo\n");
    assert!(
        asks(
            &mut g,
            1,
            "reminders_write",
            json!({"action":"delete","guid":"R-1"})
        )
        .await
    );
}

#[tokio::test]
async fn a_broken_file_never_loosens_the_mode() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    // Broken from the start: the mode from setup stays, not the yolo in the broken file.
    write_file(
        &dir,
        "version: 1\napproval_mode: yolo\nrules:\n  nonsense\n",
    );
    assert!(
        asks(
            &mut f,
            1,
            "reminders_write",
            json!({"action":"create","list_guid":"list","title":"Synthetic created"})
        )
        .await
    );
    // A valid stricter mode, then a broken edit toward yolo: the stricter mode stays.
    write_file(&dir, "version: 1\napproval_mode: reads_and_writes\n");
    assert!(asks(&mut f, 2, "reminders_lists", json!({})).await);
    write_file(&dir, "version: 1\napproval_mode: yolo\nbogus: 1\n");
    assert!(asks(&mut f, 3, "reminders_lists", json!({})).await);
    let active = permissions::peek(&f.library, &dir, Some("writes_only")).unwrap();
    assert_eq!(active.mode.as_deref(), Some("reads_and_writes"));
    assert!(active.fallback().is_some());
}

#[tokio::test]
async fn looser_settings_are_listed_until_the_person_looks() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    write_file(&dir, "version: 1\n");
    handle(&mut f.server, call(1, "reminders_lists", json!({}))).await;
    retried(|| permissions::show(&dir, true));
    let exchange = private("xchg");
    let folder_path = exchange.path().canonicalize().unwrap();
    write_file(
        &dir,
        &format!(
            "version: 1\napproval_mode: yolo\nexchange_folder: {}\n",
            folder_path.display()
        ),
    );
    handle(&mut f.server, call(2, "reminders_lists", json!({}))).await;
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let notice = &lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .expect("reported")
        .0;
    assert!(notice.contains("(and 1 earlier)"), "{notice}");
    let shown = retried(|| permissions::show(&dir, true));
    assert!(
        shown.contains("looser: approval mode: writes_only before, now yolo"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "looser: exchange folder: none before, now {}",
            folder_path.display()
        )),
        "{shown}"
    );
    assert!(
        shown.contains("Approval mode: yolo, set in permissions.yaml"),
        "{shown}"
    );
    // Stricter changes are listed, but no notice waits for them.
    write_file(&dir, "version: 1\napproval_mode: reads_and_writes\n");
    handle(&mut f.server, call(3, "reminders_lists", json!({}))).await;
    assert!(permissions::status(&f.library, &dir, Some("writes_only"))
        .iter()
        .all(|(t, _)| !t.contains("became looser")));
    let shown = retried(|| permissions::show(&dir, true));
    assert!(
        shown.contains("stricter: approval mode: yolo before, now reads_and_writes"),
        "{shown}"
    );
    assert!(shown.contains("stricter: exchange folder:"), "{shown}");
}

#[test]
fn the_exchange_folder_must_be_a_private_folder_of_your_own_and_nothing_wider() {
    use skykeep_mcp::exchange::checked;
    let state = private("state");
    let state_dir = state.path().canonicalize().unwrap();
    let base = private("xchg");
    let base_dir = base.path().canonicalize().unwrap();
    let good = base_dir.join("Exchange");
    std::fs::create_dir(&good).unwrap();
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ok = checked(good.to_str().unwrap(), &state_dir, 1024).unwrap();
    assert_eq!(ok.path, good);
    assert_eq!(ok.max_bytes, 1024);
    let missing = base_dir.join("gone");
    std::os::unix::fs::symlink(&good, base_dir.join("link")).unwrap();
    std::fs::write(base_dir.join("file"), "x").unwrap();
    let shared = base_dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let mut hidden = Vec::new();
    for name in [".ssh", ".gnupg", ".aws", "Credentials", "keyrings"] {
        let inside = base_dir.join(name).join("in");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::set_permissions(&inside, std::fs::Permissions::from_mode(0o700)).unwrap();
        hidden.push(inside);
    }
    let inside_state = state_dir.join("in");
    std::fs::create_dir(&inside_state).unwrap();
    let text = |p: &Path| p.to_str().unwrap().to_string();
    let mut cases = vec![
        (text(&missing), "exchange_folder_missing"),
        (text(&base_dir.join("link")), "exchange_folder_is_a_link"),
        (text(&base_dir.join("file")), "exchange_folder_not_a_folder"),
        (text(&shared), "exchange_folder_writable_by_others"),
        ("/".to_string(), "exchange_folder_system_folder"),
        ("/etc".to_string(), "exchange_folder_system_folder"),
        ("/usr/bin".to_string(), "exchange_folder_system_folder"),
        (text(&state_dir), "exchange_folder_private_state"),
        (text(&inside_state), "exchange_folder_private_state"),
    ];
    for inside in &hidden {
        cases.push((text(inside), "exchange_folder_hidden_or_credentials"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        if home.is_dir() {
            cases.push((text(&home), "exchange_folder_system_folder"));
            cases.push(("~".to_string(), "exchange_folder_system_folder"));
        }
    }
    // A state folder inside the exchange folder would put its files in reach of an upload.
    let holder = private("holder");
    let nested_state = holder.path().canonicalize().unwrap().join("state");
    std::fs::create_dir(&nested_state).unwrap();
    std::fs::set_permissions(&nested_state, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        checked(
            holder.path().canonicalize().unwrap().to_str().unwrap(),
            &nested_state,
            1
        )
        .unwrap_err(),
        "exchange_folder_private_state"
    );
    for (written, code) in cases {
        assert_eq!(
            checked(&written, &state_dir, 1).map(|e| e.path),
            Err(code),
            "{written}"
        );
    }
    // A folder of another user, where the test can make one.
    if unsafe { libc::geteuid() } == 0 {
        let theirs = base_dir.join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::chown(&theirs, Some(54321), Some(54321)).unwrap();
        assert_eq!(
            checked(theirs.to_str().unwrap(), &state_dir, 1).map(|e| e.path),
            Err("exchange_folder_not_yours")
        );
    }
}

#[tokio::test]
async fn a_bad_folder_in_the_file_is_refused_and_never_falls_back_to_the_older_one() {
    let f = folder(Some("writes_only")).await;
    let dir = f.path();
    let old = private("xchg-old");
    {
        let mut library = quince_core::Library::open(&dir).unwrap();
        skykeep_mcp::exchange::set_up(&mut library, Some(old.path().to_path_buf()), None).unwrap();
    }
    let library = quince_core::Library::open(&dir).unwrap();
    // No line in the file: the folder set up in the terminal is used.
    assert_eq!(
        skykeep_mcp::exchange::in_force(&library, &dir)
            .unwrap()
            .unwrap()
            .path,
        old.path().canonicalize().unwrap()
    );
    let gone = old.path().canonicalize().unwrap().with_extension("gone");
    write_file(
        &dir,
        &format!("version: 1\nexchange_folder: {}\n", gone.display()),
    );
    assert_eq!(
        skykeep_mcp::exchange::in_force(&library, &dir).map(|e| e.map(|e| e.path)),
        Err("exchange_folder_missing")
    );
    let step = skykeep_mcp::next_step("exchange_folder_missing", &dir, &Value::Null).unwrap();
    assert!(step.contains(&gone.display().to_string()), "{step}");
    assert!(step.contains("exchange_folder"), "{step}");
    assert!(step.contains("permissions.yaml"), "{step}");
    // A broken file uses no folder: not the last valid one from the file, not the older one.
    let new = private("xchg-new");
    let new_path = new.path().canonicalize().unwrap();
    write_file(
        &dir,
        &format!("version: 1\nexchange_folder: {}\n", new_path.display()),
    );
    let mut library = quince_core::Library::open(&dir).unwrap();
    permissions::load(&mut library, &dir, Some("writes_only")).unwrap();
    write_file(
        &dir,
        &format!(
            "version: 1\nexchange_folder: {}\nbogus\n",
            old.path().display()
        ),
    );
    assert_eq!(
        skykeep_mcp::exchange::in_force(&library, &dir).map(|e| e.map(|e| e.path)),
        Err("exchange_folder_file_unusable")
    );
}

/// Issue 75: while permissions.yaml can't be used, for any reason, no exchange folder is in
/// use, and the next step names the file and its mistake. A file without the line names no
/// folder, even when one was set up in the terminal before.
#[tokio::test]
async fn a_broken_or_removed_exchange_line_leaves_no_exchange_folder() {
    let f = folder(Some("writes_only")).await;
    let dir = f.path();
    let older = private("xchg-older");
    let good = private("xchg-good");
    let good_path = good.path().canonicalize().unwrap();
    let mut library = quince_core::Library::open(&dir).unwrap();
    skykeep_mcp::exchange::set_up(&mut library, Some(older.path().to_path_buf()), None).unwrap();
    let good_file = format!("version: 1\nexchange_folder: {}\n", good_path.display());
    write_file(&dir, &good_file);
    permissions::load(&mut library, &dir, Some("writes_only")).unwrap();
    let in_force = |library: &quince_core::Library| {
        skykeep_mcp::exchange::in_force(library, &dir).map(|e| e.map(|e| e.path))
    };
    assert_eq!(in_force(&library), Ok(Some(good_path.clone())));
    for (text, mistake) in [
        (
            "version: 1\nexchange_folder: rel/ex\n".to_string(),
            "line 2",
        ),
        (format!("{good_file}bogus_key: 1\n"), "line 3"),
    ] {
        write_file(&dir, &text);
        assert_eq!(
            in_force(&library),
            Err("exchange_folder_file_unusable"),
            "{text}"
        );
        // Loading the broken file changes nothing about that.
        permissions::load(&mut library, &dir, Some("writes_only")).unwrap();
        assert_eq!(in_force(&library), Err("exchange_folder_file_unusable"));
        let step =
            skykeep_mcp::next_step("exchange_folder_file_unusable", &dir, &Value::Null).unwrap();
        assert!(step.contains("permissions.yaml"), "{step}");
        assert!(step.contains(mistake), "{step}");
        assert!(!step.contains(&good_path.display().to_string()), "{step}");
        write_file(&dir, &good_file);
        permissions::load(&mut library, &dir, Some("writes_only")).unwrap();
    }
    // A file that can't be read at all.
    let file = dir.join(permissions::FILE_NAME);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(in_force(&library), Err("exchange_folder_file_unusable"));
    std::fs::remove_dir(&file).unwrap();
    // The line taken out: no folder, not the one set up before.
    write_file(&dir, "version: 1\n");
    assert_eq!(in_force(&library), Ok(None));
    permissions::load(&mut library, &dir, Some("writes_only")).unwrap();
    assert_eq!(in_force(&library), Ok(None));
    // A good file works again.
    write_file(&dir, &good_file);
    assert_eq!(in_force(&library), Ok(Some(good_path)));
}

#[test]
fn a_shortcut_edits_one_line_and_keeps_the_rest_of_the_file() {
    let dir = private("state");
    let dir = dir.path();
    let original = "# My notes about the rules\nversion: 1\napproval_mode: writes_only  # chosen in May\nrules:\n  # keep asking\n  notes_list: deny\n";
    write_file(dir, original);
    permissions::set_setting(dir, permissions::MODE_KEY, "yolo").unwrap();
    let text = std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap();
    assert_eq!(
        text,
        "# My notes about the rules\nversion: 1\napproval_mode: yolo  # chosen in May\nrules:\n  # keep asking\n  notes_list: deny\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(permissions::BACKUP_NAME)).unwrap(),
        original
    );
    let mode = std::fs::metadata(dir.join(permissions::FILE_NAME))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A setting not yet in the file goes after the version line.
    permissions::set_setting(dir, permissions::EXCHANGE_KEY, "/srv/My Exchange").unwrap();
    let text = std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap();
    let parsed = permissions::parse_settings(&text).unwrap();
    assert_eq!(parsed.exchange.as_deref(), Some("/srv/My Exchange"));
    assert_eq!(parsed.mode.as_deref(), Some("yolo"));
    assert!(text.contains("# keep asking"), "{text}");
    // No file yet: a small one is written.
    let fresh = private("fresh");
    permissions::set_setting(fresh.path(), permissions::MODE_KEY, "reads_and_writes").unwrap();
    let text = std::fs::read_to_string(fresh.path().join(permissions::FILE_NAME)).unwrap();
    assert_eq!(
        permissions::parse_settings(&text).unwrap().mode.as_deref(),
        Some("reads_and_writes")
    );
    // A broken file is left as it is, with the reason.
    let broken = "version: 1\nrules:\n  nonsense\n";
    write_file(dir, broken);
    let error = permissions::set_setting(dir, permissions::MODE_KEY, "yolo").unwrap_err();
    assert!(error.contains("line 3"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap(),
        broken
    );
    // A value the file would not accept is never written.
    write_file(dir, original);
    assert!(permissions::set_setting(dir, permissions::MODE_KEY, "everything").is_err());
    assert!(permissions::set_setting(dir, "password", "x").is_err());
    assert_eq!(
        std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap(),
        original
    );
}

#[tokio::test]
async fn status_says_where_each_setting_comes_from() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    assert!(
        lines
            .iter()
            .any(|(t, n)| t.contains("Exchange folder: not set up") && n.is_none()),
        "{lines:?}"
    );
    let exchange = private("xchg");
    let path = exchange.path().canonicalize().unwrap();
    write_file(
        &dir,
        &format!(
            "version: 1\napproval_mode: yolo\nexchange_folder: {}\n",
            path.display()
        ),
    );
    handle(&mut f.server, call(1, "reminders_lists", json!({}))).await;
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    assert!(
        lines.iter().any(|(t, _)| t.contains(&format!(
            "Exchange folder: {}, set in permissions.yaml",
            path.display()
        ))),
        "{lines:?}"
    );
    write_file(
        &dir,
        &format!(
            "version: 1\nexchange_folder: {}\n",
            path.join("gone").display()
        ),
    );
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let (text, next) = lines
        .iter()
        .find(|(t, _)| t.contains("Exchange folder"))
        .unwrap();
    assert!(text.contains("does not exist"), "{text}");
    assert!(
        next.as_deref().unwrap().contains("exchange_folder"),
        "{next:?}"
    );
}

/// `skykeep-mcp policy MODE`, run in a terminal of its own: `answer` is typed at the prompt.
/// Returns whether it succeeded and what it printed.
fn policy_command(dir: &Path, mode: &str, answer: &str) -> (bool, String) {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    let (mut master, slave) = {
        let (mut m, mut s) = (0, 0);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s)) }
    };
    let tty = slave.as_raw_fd();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"));
    command
        .arg("policy")
        .arg("--state-dir")
        .arg(dir)
        .arg(mode)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    for marker in skykeep_mcp::terminal::AGENT_MARKERS {
        command.env_remove(marker);
    }
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(tty, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    drop(slave);
    let mut said = Vec::new();
    let mut buf = [0u8; 1024];
    let mut typed = false;
    while let Ok(n) = master.read(&mut buf) {
        if n == 0 {
            break;
        }
        said.extend_from_slice(&buf[..n]);
        // Typed only once the prompt is up: the terminal drops earlier input.
        if !typed && String::from_utf8_lossy(&said).contains("Confirmation: ") {
            master.write_all(format!("{answer}\n").as_bytes()).unwrap();
            typed = true;
        }
    }
    let status = child.wait().unwrap();
    (
        status.success(),
        String::from_utf8_lossy(&said).into_owned(),
    )
}

#[test]
fn the_policy_command_writes_the_files_line_and_keeps_its_comments() {
    let state = private("state");
    let dir = state.path().canonicalize().unwrap();
    let mut server = Server::open(&dir).unwrap();
    retried(|| server.set_policy("writes_only"));
    drop(server);
    write_file(
        &dir,
        "version: 1\napproval_mode: writes_only  # my choice\nrules:\n  notes_list: deny\n",
    );
    let (ok, said) = policy_command(&dir, "yolo", "confirm");
    assert!(ok, "{said}");
    assert!(
        said.contains("permissions.yaml now says approval_mode: yolo"),
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap(),
        "version: 1\napproval_mode: yolo  # my choice\nrules:\n  notes_list: deny\n"
    );
    let library = quince_core::Library::open(&dir).unwrap();
    let stored: Value =
        serde_json::from_str(&library.state("mcp.policy").unwrap().unwrap()).unwrap();
    assert_eq!(stored["mode"], "yolo");
    // A file with a mistake: nothing is asked, stored or written.
    let broken = "version: 1\napproval_mode: yolo\nrules:\n  nonsense\n";
    write_file(&dir, broken);
    let (ok, said) = policy_command(&dir, "reads_and_writes", "confirm");
    assert!(!ok, "{said}");
    assert!(said.contains("can't be edited"), "{said}");
    assert!(!said.contains("Confirmation: "), "{said}");
    assert_eq!(
        std::fs::read_to_string(dir.join(permissions::FILE_NAME)).unwrap(),
        broken
    );
    let stored: Value =
        serde_json::from_str(&library.state("mcp.policy").unwrap().unwrap()).unwrap();
    assert_eq!(stored["mode"], "yolo");
}

#[tokio::test]
async fn a_write_queued_under_a_looser_mode_is_set_aside_when_the_file_gets_stricter() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    write_file(&dir, "version: 1\napproval_mode: yolo\n");
    let create = |title: &str| json!({"action":"create","list_guid":"list","title":title});
    assert!(!asks(&mut f, 1, "reminders_write", create("Queued under yolo")).await);
    let library = quince_core::Library::open(&dir).unwrap();
    let first = library.outbox_all_pending().unwrap().remove(0);
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &first),
        Ok(())
    );
    // Looser again changes nothing for it.
    write_file(
        &dir,
        "version: 1\napproval_mode: yolo\nrules:\n  notes_list: deny\n",
    );
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &first),
        Ok(())
    );
    // Stricter: it is never sent, and the next write sets it aside.
    write_file(
        &dir,
        "version: 1\napproval_mode: writes_only\nrules:\n  reminders_write.create: allow\n",
    );
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &first),
        Err("write_mode_stricter")
    );
    assert!(
        !asks(
            &mut f,
            2,
            "reminders_write",
            create("Queued under writes_only")
        )
        .await
    );
    let pending = library.outbox_all_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].id, first.id);
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &pending[0]),
        Ok(())
    );
    let refused = library.outbox_refused_brief().unwrap();
    assert!(
        refused
            .iter()
            .any(|r| r.id == first.id && r.reason == "mcp_quarantine:write_mode_stricter"),
        "{:?}",
        refused.iter().map(|r| &r.reason).collect::<Vec<_>>()
    );
}

/// A suggested mkdir or chmod line names the folder so the shell finds it: a path the file
/// writes with ~/ is expanded before it is quoted.
#[test]
fn suggested_commands_never_quote_a_tilde() {
    if std::env::var_os("HOME").filter(|h| !h.is_empty()).is_none() {
        return;
    }
    let state = private("state");
    let dir = state.path().canonicalize().unwrap();
    write_file(&dir, "version: 1\nexchange_folder: ~/Sky Missing Folder\n");
    for code in [
        "exchange_folder_missing",
        "exchange_folder_writable_by_others",
    ] {
        let step = skykeep_mcp::next_step(code, &dir, &Value::Null).unwrap();
        assert!(!step.contains("'~") && !step.contains("\"~"), "{step}");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert!(
            step.contains(&home.join("Sky Missing Folder").display().to_string()),
            "{step}"
        );
    }
}

/// A folder with no stored approval mode takes the file's mode as its stored one at the next
/// call, so queued writes carry a grant and a later stricter mode sets them aside.
#[tokio::test]
async fn a_mode_in_the_file_binds_queued_writes_in_a_folder_without_a_stored_mode() {
    let mut f = folder(None).await;
    let dir = f.path();
    write_file(&dir, "version: 1\napproval_mode: yolo\n");
    let create = |title: &str| json!({"action":"create","list_guid":"list","title":title});
    assert!(!asks(&mut f, 1, "reminders_write", create("Queued under yolo")).await);
    let library = quince_core::Library::open(&dir).unwrap();
    let stored: Value =
        serde_json::from_str(&library.state("mcp.policy").unwrap().unwrap()).unwrap();
    assert_eq!(stored["mode"], "yolo");
    let first = library.outbox_all_pending().unwrap().remove(0);
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &first),
        Ok(())
    );
    write_file(
        &dir,
        "version: 1\napproval_mode: reads_and_writes\nrules:\n  reminders_write.create: allow\n",
    );
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, &dir, &first),
        Err("write_mode_stricter")
    );
    assert!(!asks(&mut f, 2, "reminders_write", create("Second")).await);
    let refused = library.outbox_refused_brief().unwrap();
    assert!(
        refused
            .iter()
            .any(|r| r.id == first.id && r.reason == "mcp_quarantine:write_mode_stricter"),
        "{:?}",
        refused.iter().map(|r| &r.reason).collect::<Vec<_>>()
    );
}

/// A mode taken over from the file counts only while the file names one: once the line is
/// gone, the folder is back to its setup choice (here no approval mode, with every change
/// asking), and the change is listed as stricter.
#[tokio::test]
async fn a_mode_taken_from_the_file_stops_counting_when_its_line_is_removed() {
    let mut f = folder(None).await;
    let dir = f.path();
    f.library
        .set_state(skykeep_mcp::status::NO_MODE_KEY, "1")
        .unwrap();
    f.library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    write_file(&dir, "version: 1\napproval_mode: yolo\n");
    assert!(!asks(&mut f, 1, "reminders_lists", json!({})).await);
    let create = json!({"action":"create","list_guid":"list","title":"Synthetic"});
    assert!(!asks(&mut f, 2, "reminders_write", create.clone()).await);
    retried(|| permissions::show(&dir, true));
    write_file(&dir, "version: 1\n");
    assert!(asks(&mut f, 3, "reminders_write", create).await);
    let shown = retried(|| permissions::show(&dir, true));
    assert!(
        shown.contains("stricter: approval mode: yolo before, now none"),
        "{shown}"
    );
}

/// Changes waiting from before a mode was taken over from the file are set aside, and the
/// tool result says where to see them.
#[tokio::test]
async fn taking_over_the_files_mode_says_where_set_aside_changes_are() {
    let mut f = folder(None).await;
    let dir = f.path();
    let create = json!({"action":"create","list_guid":"list","title":"Waiting"});
    assert!(!asks(&mut f, 1, "reminders_write", create).await);
    write_file(&dir, "version: 1\napproval_mode: writes_only\n");
    let reply = handle(&mut f.server, call(2, "reminders_lists", json!({}))).await;
    let body: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let notice = body["set_aside"].as_str().unwrap_or_default();
    assert!(notice.contains("1 waiting change"), "{body}");
    assert!(notice.contains("skykeep-mcp quarantine"), "{body}");
    // Said once.
    let reply = handle(&mut f.server, call(3, "reminders_lists", json!({}))).await;
    assert!(!reply.to_string().contains("set_aside"), "{reply}");
}
