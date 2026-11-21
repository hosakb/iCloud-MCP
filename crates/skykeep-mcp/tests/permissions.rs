//! Per-tool permissions from permissions.yaml: live reload, each effect, the approval card,
//! the terminal approval, the log and the reports. Synthetic data only, no network.
use serde_json::{json, Value};
use skykeep_mcp::permissions::{self, Effect, Removal};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const SENTINEL: &str = "SYNTHETIC-RULES-SENTINEL";

struct Folder {
    dir: tempfile::TempDir,
    server: Server,
    library: quince_core::Library,
}

impl Folder {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// A private folder bound to a synthetic account, with a reminder list and one reminder, in
/// `mode` (`None`: no approval mode), after an initialize with or without approval forms.
async fn folder(mode: Option<&str>, forms: bool) -> Folder {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
        server.set_policy(mode).unwrap();
    }
    let capabilities = if forms {
        json!({"elicitation":{"form":{}}})
    } else {
        json!({})
    };
    server
        .handle(json!({"id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":capabilities}}))
        .await;
    Folder {
        dir,
        server,
        library,
    }
}

fn write_rules(dir: &Path, text: &str) {
    let path = dir.join(permissions::FILE_NAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn call(id: i64, name: &str, args: Value) -> Value {
    json!({"id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

fn body(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn answer(form: &Value, decision: Option<&str>) -> Value {
    let content = match decision {
        Some(d) => json!({"decision":d}),
        None => json!({}),
    };
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":content}})
}

fn choices(form: &Value) -> Vec<String> {
    form["params"]["requestedSchema"]["properties"]["decision"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["const"].as_str().unwrap().to_string())
        .collect()
}

fn create_reminder() -> Value {
    json!({"action":"create","list_guid":"list","title":"Synthetic created"})
}

#[test]
fn every_valid_name_takes_every_effect_and_names_come_from_the_tool_list() {
    let names = permissions::names();
    for expected in [
        "notes_write",
        "notes_write.delete",
        "reminders_write.create",
        "reminders_write.delete",
        "contacts_get",
        "drive_refresh",
    ] {
        assert!(names.iter().any(|n| n == expected), "{expected} missing");
    }
    assert!(!names
        .iter()
        .any(|n| n == "auth_status" || n == "sync_status"));
    for target in permissions::targets() {
        let changes = names
            .iter()
            .any(|n| n.starts_with(&format!("{}.", target.tool)));
        for (text, effect) in [
            ("allow", Effect::Allow),
            ("{ask: client}", Effect::AskClient),
            ("{ask: terminal}", Effect::AskTerminal),
            ("deny", Effect::Deny),
        ] {
            let parsed =
                permissions::parse(&format!("version: 1\nrules:\n  {}: {text}\n", target.name));
            if effect == Effect::AskTerminal && !changes {
                assert!(parsed.unwrap_err().contains("can't use ask: terminal"));
                continue;
            }
            let rules = parsed.unwrap();
            assert_eq!(
                permissions::rule_for(&rules, &target.tool, target.action.as_deref()),
                Some(effect),
                "{}",
                target.name
            );
        }
    }
    // The nested form and comments.
    let rules = permissions::parse(
        "# top\nversion: 1\nrules:\n  notes_write.delete:   # deletions\n    ask: terminal\n  notes_write: deny\n",
    )
    .unwrap();
    assert_eq!(rules["notes_write.delete"], Effect::AskTerminal);
    // The action's own entry wins over the tool's.
    assert_eq!(
        permissions::rule_for(&rules, "notes_write", Some("delete")),
        Some(Effect::AskTerminal)
    );
    assert_eq!(
        permissions::rule_for(&rules, "notes_write", Some("create")),
        Some(Effect::Deny)
    );
}

#[test]
fn unknown_names_and_bad_values_are_refused_with_a_plain_message() {
    let error = permissions::parse("rules:\n  notes_write.explode: allow\n").unwrap_err();
    assert!(
        error.starts_with(
            "line 2: notes_write.explode is not a Skykeep tool or action. Valid names: "
        ),
        "{error}"
    );
    assert!(error.contains("reminders_write.create"), "{error}");
    for (text, part) in [
        (
            "rules:\n  notes_write.delete: ask: terminal\n",
            "{ask: terminal}",
        ),
        ("rules:\n  notes_write.delete: maybe\n", "not a choice"),
        ("rules:\n\tnotes_list: allow\n", "tabs"),
        (
            "rules:\n  notes_list: allow\n  notes_list: deny\n",
            "listed twice",
        ),
        ("version: 2\n", "version"),
        ("permissions:\n  notes_list: allow\n", "unknown setting"),
        ("rules:\n  notes_write.delete:\n", "has no value"),
        (
            "rules:\n  \u{1b}[31mnotes: allow\n",
            "is not a Skykeep tool",
        ),
    ] {
        let error = permissions::parse(text).unwrap_err();
        assert!(error.contains(part), "{text:?}: {error}");
        assert!(
            !error.contains('\u{1b}'),
            "control characters are never echoed"
        );
    }
}

#[test]
fn removals_are_classified_in_one_place() {
    assert_eq!(
        permissions::removal_of("drive_write", "delete_forever"),
        Removal::Permanent
    );
    assert_eq!(
        permissions::removal_of("drive_write", "trash"),
        Removal::Recoverable
    );
    assert_eq!(
        permissions::removal_of("drive_write", "rename"),
        Removal::None
    );
    assert_eq!(
        permissions::removal_of("notes_write", "delete"),
        Removal::Recoverable
    );
    assert_eq!(
        permissions::removal_of("reminders_write", "delete"),
        Removal::Permanent
    );
    assert_eq!(
        permissions::removal("notes_write", &json!({"action":"save","body":"  "})),
        Removal::Permanent
    );
    assert_eq!(
        permissions::removal(
            "notes_write",
            &json!({"action":"move","folder_guid":quince_core::notes::TRASH_FOLDER})
        ),
        Removal::Recoverable
    );
}

#[tokio::test]
async fn deny_refuses_every_name_with_a_clear_error_and_runs_nothing() {
    let mut f = folder(Some("writes_only"), true).await;
    for target in permissions::targets() {
        write_rules(f.path(), &format!("rules:\n  {}: deny\n", target.name));
        let args = match &target.action {
            Some(action) => json!({"action":action}),
            None => json!({}),
        };
        let reply = f.server.handle(call(1, &target.tool, args)).await.unwrap();
        let result = body(&reply);
        assert_eq!(reply["result"]["isError"], true, "{}", target.name);
        assert_eq!(result["error"], "denied_by_permissions", "{}", target.name);
        assert!(result["message"]
            .as_str()
            .unwrap()
            .contains("sets this action to deny"));
    }
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    // A Notes save that empties a note follows the delete rule.
    write_rules(
        f.path(),
        "rules:\n  notes_write.save: allow\n  notes_write.delete: deny\n",
    );
    let reply = f
        .server
        .handle(call(
            2,
            "notes_write",
            json!({"action":"save","guid":"N","body":""}),
        ))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "denied_by_permissions");
}

#[tokio::test]
async fn allow_runs_without_a_card_and_keeps_the_send_authorization() {
    let mut f = folder(Some("writes_only"), true).await;
    // Without a rule, the mode asks.
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create");
    f.server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
        .await;
    write_rules(f.path(), "rules:\n  reminders_write.create: allow\n");
    let reply = f
        .server
        .handle(call(2, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    let pending = f.library.outbox_all_pending().unwrap();
    assert_eq!(pending.len(), 1);
    skykeep_mcp::sync::authorized(&f.library, &f.path().canonicalize().unwrap(), &pending[0])
        .unwrap();
    // A read under reads_and_writes.
    let mut f = folder(Some("reads_and_writes"), true).await;
    write_rules(f.path(), "rules:\n  reminders_lists: allow\n");
    let reply = f
        .server
        .handle(call(3, "reminders_lists", json!({})))
        .await
        .unwrap();
    assert_eq!(body(&reply)["total"], 1, "{reply}");
}

#[tokio::test]
async fn ask_client_shows_the_card_where_forms_exist_and_says_what_to_do_where_not() {
    let mut f = folder(Some("yolo"), true).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create:\n    ask: client\n",
    );
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create");
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    let reply = f
        .server
        .handle(answer(&form, Some("approve")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    assert!(body(&reply).get("permission_rule").is_none());

    let mut f = folder(Some("yolo"), false).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create: {ask: client}\n",
    );
    let reply = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["error"], "approval_requires_form_client");
    let message = result["message"].as_str().unwrap();
    for option in ["ask: terminal", "allow", "deny", "skykeep-mcp approve"] {
        assert!(message.contains(option), "{message}");
    }
    assert!(!message.contains("Hermes"));
    assert_eq!(f.library.outbox_count().unwrap(), 0);

    // A folder without an approval mode can ask in the client too.
    let mut f = folder(None, true).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create: {ask: client}\n",
    );
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create");
    let reply = f
        .server
        .handle(answer(&form, Some("approve")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
}

#[tokio::test]
async fn ask_terminal_waits_for_skykeep_approve_with_and_without_an_approval_mode() {
    for mode in [Some("yolo"), None] {
        let mut f = folder(mode, false).await;
        write_rules(
            f.path(),
            "rules:\n  reminders_write.create: {ask: terminal}\n",
        );
        let reply = f
            .server
            .handle(call(1, "reminders_write", create_reminder()))
            .await
            .unwrap();
        let result = body(&reply);
        assert_eq!(result["status"], "approval_required", "{reply}");
        assert_eq!(result["approval"], "terminal");
        assert!(result["next"]
            .as_str()
            .unwrap()
            .contains("skykeep-mcp approve --state-dir"));
        assert_eq!(f.library.outbox_count().unwrap(), 0);
        let id = result["request_id"].as_str().unwrap();
        assert!(f
            .server
            .approval_details(id)
            .unwrap()
            .contains("Synthetic created"));
        let approved = f.server.approve_request(id).await.unwrap();
        assert_eq!(approved["status"], "queued", "{mode:?}");
        assert_eq!(f.library.outbox_count().unwrap(), 1);
        assert!(f.server.approve_request(id).await.is_err(), "used once");
        if mode.is_some() {
            let pending = f.library.outbox_all_pending().unwrap();
            skykeep_mcp::sync::authorized(
                &f.library,
                &f.path().canonicalize().unwrap(),
                &pending[0],
            )
            .unwrap();
        }
        // A request made before the file said deny is refused.
        let reply = f
            .server
            .handle(call(2, "reminders_write", create_reminder()))
            .await
            .unwrap();
        let id = body(&reply)["request_id"].as_str().unwrap().to_string();
        write_rules(f.path(), "rules:\n  reminders_write.create: deny\n");
        assert_eq!(
            f.server.approve_request(&id).await,
            Err("denied_by_permissions")
        );
        assert_eq!(f.library.outbox_count().unwrap(), 1);
    }
    // A request from before a policy change stays refused.
    let mut f = folder(Some("yolo"), false).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create: {ask: terminal}\n",
    );
    let reply = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    let id = body(&reply)["request_id"].as_str().unwrap().to_string();
    f.server.set_policy("yolo").unwrap();
    assert_eq!(
        f.server.approve_request(&id).await,
        Err("approval_policy_changed")
    );
    assert_eq!(f.library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn edits_count_at_the_next_call_and_an_invalid_file_falls_back_to_the_mode() {
    let mut f = folder(Some("writes_only"), true).await;
    let lists = |id| call(id, "reminders_lists", json!({}));
    assert_eq!(body(&f.server.handle(lists(1)).await.unwrap())["total"], 1);
    write_rules(f.path(), "rules:\n  reminders_lists: deny\n");
    assert_eq!(
        body(&f.server.handle(lists(2)).await.unwrap())["error"],
        "denied_by_permissions"
    );
    write_rules(f.path(), "rules:\n  reminders_lists: {ask: client}\n");
    assert_eq!(
        f.server.handle(lists(3)).await.unwrap()["method"],
        "elicitation/create"
    );
    let mut f = folder(Some("writes_only"), true).await;
    for broken in [
        "rules:\n  reminders_lists: deny\n  nonsense\n",
        "rules:\n  reminders_lists: deny\n  notes_write.explode: allow\n",
    ] {
        write_rules(f.path(), broken);
        // The deny inside the broken file does not count: the mode decides.
        assert_eq!(body(&f.server.handle(lists(4)).await.unwrap())["total"], 1);
    }
    let dir = f.path().canonicalize().unwrap();
    let lines = permissions::status(&mut f.library, &dir, Some("writes_only"));
    assert!(
        lines
            .iter()
            .any(|(t, n)| t.contains("can't be used") && n.is_some()),
        "{lines:?}"
    );
    let shown = permissions::show(&dir, false).unwrap();
    assert!(
        shown.contains("The file can't be used: line 3: notes_write.explode"),
        "{shown}"
    );
    // A file others may change, a link and an oversized file are ignored.
    write_rules(f.path(), "rules:\n  reminders_lists: deny\n");
    let path = f.path().join(permissions::FILE_NAME);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
    assert!(
        f.server.handle(lists(5)).await.unwrap()["result"]["isError"] == true,
        "the folder check refuses the file"
    );
    std::fs::remove_file(&path).unwrap();
    let elsewhere = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(elsewhere.path(), "rules:\n  reminders_lists: deny\n").unwrap();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), other.path().join(permissions::FILE_NAME))
        .unwrap();
    assert!(matches!(
        permissions::read(other.path()).0,
        permissions::Source::Invalid(_)
    ));
    write_rules(
        f.path(),
        &format!("rules:\n  reminders_lists: deny\n#{}\n", "x".repeat(70000)),
    );
    assert_eq!(body(&f.server.handle(lists(6)).await.unwrap())["total"], 1);
    // A file that only others can't change, but anyone can read, is used.
    write_rules(f.path(), "rules:\n  reminders_lists: deny\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        body(&f.server.handle(lists(7)).await.unwrap())["error"],
        "denied_by_permissions"
    );
}

#[tokio::test]
async fn a_looser_change_is_reported_until_the_person_looks() {
    let mut f = folder(Some("writes_only"), true).await;
    let dir = f.path().canonicalize().unwrap();
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: {ask: terminal}\n",
    );
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    permissions::show(&dir, true).unwrap();
    assert!(
        permissions::status(&mut f.library, &dir, Some("writes_only"))
            .iter()
            .all(|(_, next)| next.is_none())
    );
    write_rules(f.path(), "rules:\n  reminders_write.delete: allow\n");
    f.server.handle(call(2, "reminders_lists", json!({}))).await;
    let lines = permissions::status(&mut f.library, &dir, Some("writes_only"));
    let looser = lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .expect("reported");
    assert!(looser.0.contains("reminders_write.delete: waits for skykeep-mcp approve in your terminal before, now runs without asking"), "{}", looser.0);
    assert!(looser.1.as_deref().unwrap().contains("permissions show"));
    let shown = permissions::show(&dir, true).unwrap();
    assert!(shown.contains("looser: reminders_write.delete"), "{shown}");
    assert!(shown.contains("Last change loaded: "), "{shown}");
    assert!(
        permissions::status(&mut f.library, &dir, Some("writes_only"))
            .iter()
            .all(|(t, _)| !t.contains("became looser"))
    );
    // Removing the file is a change too.
    std::fs::remove_file(f.path().join(permissions::FILE_NAME)).unwrap();
    let shown = permissions::show(&dir, false).unwrap();
    assert!(shown.contains("the file was removed"), "{shown}");
}

#[tokio::test]
async fn deletions_allowed_by_the_file_are_logged_without_content() {
    let mut f = folder(Some("writes_only"), true).await;
    write_rules(f.path(), "rules:\n  reminders_write.delete: allow\n");
    let reply = f
        .server
        .handle(call(
            1,
            "reminders_write",
            json!({"action":"delete","guid":"R-1"}),
        ))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    let log = std::fs::read_to_string(f.path().join(permissions::LOG_NAME)).unwrap();
    let line: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(line["tool"], "reminders_write");
    assert_eq!(line["action"], "delete");
    assert!(!log.contains("R-1") && !log.contains("Synthetic"), "{log}");
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(f.path().join(permissions::LOG_NAME)).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    let shown = permissions::show(&f.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        shown.contains("ran without asking: reminders_write.delete"),
        "{shown}"
    );
}

#[tokio::test]
async fn the_card_approves_once_denies_or_always_allows() {
    let mut f = folder(Some("writes_only"), true).await;
    let path = f.path().join(permissions::FILE_NAME);
    // Approve this time: runs, saves nothing.
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert!(form["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Always allow saves reminders_write.create: allow"));
    let reply = f
        .server
        .handle(answer(&form, Some("approve")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued");
    assert!(!path.exists());
    // Deny, no choice and a cancelled card: nothing runs.
    for response in [Some(Some("deny")), Some(None), None] {
        let form = f
            .server
            .handle(call(2, "reminders_write", create_reminder()))
            .await
            .unwrap();
        let reply = match response {
            Some(decision) => f.server.handle(answer(&form, decision)).await.unwrap(),
            None => f
                .server
                .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
                .await
                .unwrap(),
        };
        assert_eq!(body(&reply)["error"], "approval_denied");
    }
    assert_eq!(f.library.outbox_count().unwrap(), 1);
    // Always allow: runs, saves the rule, and the next call needs no card.
    let form = f
        .server
        .handle(call(3, "reminders_write", create_reminder()))
        .await
        .unwrap();
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["status"], "queued");
    assert_eq!(
        result["permission_rule"],
        json!({"saved":true,"rule":"reminders_write.create: allow"})
    );
    let saved = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        permissions::parse(&saved).unwrap()["reminders_write.create"],
        Effect::Allow
    );
    let reply = f
        .server
        .handle(call(4, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    assert_eq!(f.library.outbox_count().unwrap(), 3);
    let shown = permissions::show(&f.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        shown.contains(
            "reminders_write.create: allow, runs without asking, always allowed from a card on "
        ),
        "{shown}"
    );
    assert!(
        shown.contains("always allowed from a card: reminders_write.create"),
        "{shown}"
    );
}

#[tokio::test]
async fn always_allow_with_an_invalid_file_runs_once_and_says_the_rule_was_not_saved() {
    let mut f = folder(Some("writes_only"), true).await;
    let broken = "rules:\n  nonsense\n";
    write_rules(f.path(), broken);
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["status"], "queued");
    assert_eq!(result["permission_rule"]["saved"], false);
    assert!(result["permission_rule"]["reason"]
        .as_str()
        .unwrap()
        .contains("the file has a mistake"));
    assert_eq!(
        std::fs::read_to_string(f.path().join(permissions::FILE_NAME)).unwrap(),
        broken
    );
}

#[tokio::test]
async fn permanent_deletions_never_offer_always_allow() {
    let mut f = folder(Some("writes_only"), true).await;
    let delete = json!({"action":"delete","guid":"R-1"});
    let form = f
        .server
        .handle(call(1, "reminders_write", delete.clone()))
        .await
        .unwrap();
    assert_eq!(choices(&form), ["approve", "deny"]);
    assert!(!form["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Always allow"));
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "approval_denied");
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    assert!(!f.path().join(permissions::FILE_NAME).exists());
}

#[test]
fn a_saved_rule_keeps_comments_order_and_a_backup_and_writes_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let original = "# My rules\nversion: 1\nrules:\n  # deletions first\n  notes_write.delete:   # careful\n    ask: terminal\n  reminders_write.create:  {ask: client}  # for now\n  contacts_get: deny\n# end\n";
    write_rules(dir.path(), original);
    let print = permissions::read(dir.path()).1;
    permissions::save_allow(dir.path(), "reminders_write.create", &print).unwrap();
    let saved = std::fs::read_to_string(dir.path().join(permissions::FILE_NAME)).unwrap();
    assert_eq!(
        saved,
        "# My rules\nversion: 1\nrules:\n  # deletions first\n  notes_write.delete:   # careful\n    ask: terminal\n  reminders_write.create: allow  # for now\n  contacts_get: deny\n# end\n"
    );
    let print = permissions::read(dir.path()).1;
    permissions::save_allow(dir.path(), "notes_write.delete", &print).unwrap();
    permissions::save_allow(dir.path(), "notes_list", &permissions::read(dir.path()).1).unwrap();
    let saved = std::fs::read_to_string(dir.path().join(permissions::FILE_NAME)).unwrap();
    assert_eq!(
        saved,
        "# My rules\nversion: 1\nrules:\n  # deletions first\n  notes_write.delete: allow   # careful\n  reminders_write.create: allow  # for now\n  contacts_get: deny\n  notes_list: allow\n# end\n"
    );
    use std::os::unix::fs::MetadataExt;
    for name in [permissions::FILE_NAME, permissions::BACKUP_NAME] {
        let meta = std::fs::symlink_metadata(dir.path().join(name)).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.mode() & 0o777, 0o600, "{name}");
    }
    let backup = std::fs::read_to_string(dir.path().join(permissions::BACKUP_NAME)).unwrap();
    assert!(backup.contains("notes_write.delete: allow") && !backup.contains("notes_list"));
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // A missing file is created with just the rule.
    let empty = tempfile::tempdir().unwrap();
    permissions::save_allow(empty.path(), "notes_list", "missing").unwrap();
    assert_eq!(
        std::fs::read_to_string(empty.path().join(permissions::FILE_NAME)).unwrap(),
        "version: 1\nrules:\n  notes_list: allow\n"
    );
}

#[tokio::test]
async fn an_edit_while_the_card_is_open_wins_and_nothing_runs() {
    let dir = tempfile::tempdir().unwrap();
    write_rules(dir.path(), "rules:\n  contacts_get: deny\n");
    let stale = permissions::read(dir.path()).1;
    write_rules(
        dir.path(),
        "rules:\n  contacts_get: deny\n  notes_list: deny\n",
    );
    let error = permissions::save_allow(dir.path(), "notes_list", &stale).unwrap_err();
    assert!(error.contains("changed while the card was open"), "{error}");
    assert!(
        std::fs::read_to_string(dir.path().join(permissions::FILE_NAME))
            .unwrap()
            .contains("notes_list: deny")
    );

    let mut f = folder(Some("writes_only"), true).await;
    let form = f
        .server
        .handle(call(1, "reminders_write", create_reminder()))
        .await
        .unwrap();
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create: {ask: client}\n",
    );
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "approval_policy_changed");
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    assert!(
        std::fs::read_to_string(f.path().join(permissions::FILE_NAME))
            .unwrap()
            .contains("ask: client")
    );
}

#[tokio::test]
async fn no_mcp_tool_reads_or_changes_the_rules() {
    let mut f = folder(Some("yolo"), true).await;
    write_rules(
        f.path(),
        &format!("# {SENTINEL}\nrules:\n  notes_list: allow\n"),
    );
    let list = f
        .server
        .handle(json!({"id":1,"method":"tools/list"}))
        .await
        .unwrap();
    for tool in list["result"]["tools"].as_array().unwrap() {
        let text = tool.to_string().to_lowercase();
        assert!(
            !text.contains("permission") && !text.contains(".yaml"),
            "{tool}"
        );
    }
    for name in [
        "permissions_show",
        "permissions_apply",
        "permissions_set",
        "approve",
    ] {
        let reply = f.server.handle(call(2, name, json!({}))).await.unwrap();
        assert_eq!(body(&reply)["error"], "unknown_tool", "{name}");
    }
    for name in ["auth_status", "sync_status", "notes_list"] {
        let reply = f.server.handle(call(3, name, json!({}))).await.unwrap();
        assert!(!reply.to_string().contains(SENTINEL), "{name}");
    }
}

#[test]
fn the_starter_file_asks_before_deletions_and_lists_every_name() {
    for (mode, ask) in [
        (Some("writes_only"), Effect::AskClient),
        (None, Effect::AskTerminal),
    ] {
        let text = permissions::starter(mode);
        let rules = permissions::parse(&text).unwrap();
        assert_eq!(rules["notes_write.delete"], ask);
        assert_eq!(rules["reminders_write.delete"], ask);
        assert!(rules.keys().all(|k| k.ends_with("delete")), "{rules:?}");
        for name in permissions::names() {
            assert!(text.contains(&format!("#   {name}\n")), "{name}");
        }
    }
}
