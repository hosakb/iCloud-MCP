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
            // Not an action: it lets a change run or refuses it, and asks nobody.
            if target.name == "calendar_write.send_invitations"
                && matches!(effect, Effect::AskClient | Effect::AskTerminal)
            {
                assert!(parsed.unwrap_err().contains("takes allow or deny only"));
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
    assert!(error.starts_with("line 2: notes_write.explode is not an iCloud-MCP tool or action; the closest valid name is notes_write"), "{error}");
    assert!(error.ends_with('.'), "{error}");
    assert!(!error.contains("permissions show"), "{error}");
    let error = permissions::parse("rules:\n  reminder_write.creat: allow\n").unwrap_err();
    assert!(
        error.ends_with("there is no tool named reminder_write; did you mean reminders_write?"),
        "{error}"
    );
    // A typo of a deletion names the actions that really remove something.
    let error = permissions::parse("rules:\n  drive_write.delete: deny\n").unwrap_err();
    assert!(
        error.ends_with("the actions of drive_write that remove something are drive_write.trash and drive_write.delete_forever."),
        "{error}"
    );
    let error = permissions::parse("rules:\n  notes_write.remove: deny\n").unwrap_err();
    assert!(
        error.ends_with("the action of notes_write that removes something is notes_write.delete."),
        "{error}"
    );
    // An unknown tool is said to be unknown, never another tool's action.
    let error = permissions::parse("rules:\n  mail_write.create: allow\n").unwrap_err();
    assert!(
        error.ends_with("there is no tool named mail_write."),
        "{error}"
    );
    assert!(!error.contains("reminders_write"), "{error}");
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
        (
            "notes_list: allow\n",
            "notes_list must be indented under rules:",
        ),
        ("rules:\n  notes_write.delete:\n", "has no value"),
        (
            "rules:\n  \u{1b}[31mnotes: allow\n",
            "is not an iCloud-MCP tool",
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
fn removals_are_classified_by_whole_words() {
    for action in ["undelete", "deleted_view", "trashed_count", "restore"] {
        assert_eq!(
            permissions::removal_of("drive_write", action),
            Removal::None,
            "{action}"
        );
    }
    assert_eq!(
        permissions::removal_of("drive_write", "move_to_trash"),
        Removal::Recoverable
    );
    assert_eq!(
        permissions::removal_of("drive_write", "purge_all"),
        Removal::Permanent
    );
}

#[test]
fn permissions_show_errors_say_what_to_do_for_this_command() {
    let busy = permissions::plain("state_busy_retry");
    assert!(busy.contains("permissions show again"), "{busy}");
    for code in [
        "state_unavailable",
        "unsafe_state_contents",
        "state_must_be_owned_and_private",
    ] {
        let text = permissions::plain(code);
        assert_ne!(text, code);
        assert!(
            !text.contains("setup again") && !text.contains("Choose another folder"),
            "{text}"
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
            .contains("deny in the permissions file refuses this action"));
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
    assert!(result.get("request_id").is_none(), "{result}");
    let message = result["message"].as_str().unwrap();
    for said in ["approval card", "nothing ran", "Hermes"] {
        assert!(message.contains(said), "{message}");
    }
    // Nothing sends the person to the terminal by default.
    for not_said in ["ask: terminal", "skykeep-mcp approve"] {
        assert!(!message.contains(not_said), "{message}");
    }
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
async fn edits_count_at_the_next_call_and_an_unusable_file_keeps_the_last_valid_rules() {
    let mut f = folder(Some("writes_only"), true).await;
    let lists = |id| call(id, "reminders_lists", json!({}));
    assert_eq!(body(&f.server.handle(lists(1)).await.unwrap())["total"], 1);
    write_rules(f.path(), "rules:\n  reminders_lists: {ask: client}\n");
    let form = f.server.handle(lists(2)).await.unwrap();
    assert_eq!(form["method"], "elicitation/create");
    f.server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
        .await;
    write_rules(f.path(), "rules:\n  reminders_lists: deny\n");
    assert_eq!(
        body(&f.server.handle(lists(3)).await.unwrap())["error"],
        "denied_by_permissions"
    );
    let path = f.path().join(permissions::FILE_NAME);
    let d = f.path().to_path_buf();
    let outside = tempfile::tempdir().unwrap();
    let unusable: Vec<Box<dyn Fn()>> = vec![
        Box::new(|| write_rules(&d, "rules:\n  reminders_lists: allow\n  nonsense\n")),
        Box::new(|| {
            write_rules(
                &d,
                "rules:\n  reminders_lists: allow\n  notes_write.explode: allow\n",
            )
        }),
        Box::new(|| write_rules(&d, "rules:\n\treminders_lists: allow\n")),
        Box::new(|| {
            write_rules(&d, "rules:\n  reminders_lists: allow\n");
            std::fs::write(&path, b"rules:\n  reminders_lists: allow \xff\n").unwrap();
        }),
        Box::new(|| {
            write_rules(
                &d,
                &format!("rules:\n  reminders_lists: allow\n#{}\n", "x".repeat(70000)),
            )
        }),
        Box::new(|| {
            write_rules(&d, "rules:\n  reminders_lists: allow\n");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        }),
        Box::new(|| {
            std::fs::remove_file(&path).unwrap();
            let elsewhere = outside.path().join("elsewhere.txt");
            std::fs::write(&elsewhere, "rules:\n  reminders_lists: allow\n").unwrap();
            std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        }),
    ];
    for (n, make) in unusable.iter().enumerate() {
        make();
        // The deny from the last valid file still counts: an unusable file never loosens.
        let reply = f.server.handle(lists(10 + n as i64)).await.unwrap();
        assert_eq!(
            body(&reply)["error"],
            "denied_by_permissions",
            "case {n}: {reply}"
        );
        let _ = std::fs::remove_file(&path);
        write_rules(f.path(), "rules:\n  reminders_lists: deny\n");
        f.server.handle(lists(30)).await;
    }
    // A shared file does not stop the folder, and the fix names the exact command.
    write_rules(f.path(), "rules:\n  reminders_lists: allow\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
    let reply = f
        .server
        .handle(call(40, "sync_status", json!({})))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let dir = f.path().canonicalize().unwrap();
    let chmod = format!("chmod 600 {}", dir.join(permissions::FILE_NAME).display());
    assert!(
        result["permissions_file"]
            .as_str()
            .unwrap()
            .contains(&chmod),
        "{result}"
    );
    assert!(result["permissions_file"]
        .as_str()
        .unwrap()
        .contains("keeps the last valid rules"));
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    assert!(
        lines.iter().any(|(t, n)| t.contains(&chmod) && n.is_some()),
        "{lines:?}"
    );
    let shown = permissions::show(&dir, false).unwrap();
    assert!(
        shown.contains("Last valid rules, still in use:\n  reminders_lists: deny"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_broken_file_with_no_earlier_rules_makes_every_deletion_ask() {
    // yolo never asks, but not while the file is broken and nothing valid came before.
    let mut f = folder(Some("yolo"), true).await;
    write_rules(f.path(), "rules:\n\treminders_write.delete: allow\n");
    let delete = json!({"action":"delete","guid":"R-1"});
    let form = f
        .server
        .handle(call(1, "reminders_write", delete.clone()))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    f.server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
        .await;
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    // Everything else follows the mode and says why.
    let reply = f
        .server
        .handle(call(2, "reminders_write", create_reminder()))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["status"], "queued");
    assert!(result["permissions_file"]
        .as_str()
        .unwrap()
        .contains("asks before every deletion"));
    // A tab line under yolo after a valid deny: the deny stays.
    let mut f = folder(Some("yolo"), true).await;
    write_rules(f.path(), "rules:\n  reminders_write.delete: deny\n");
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    write_rules(f.path(), "rules:\n\treminders_write.delete: allow\n");
    let reply = f
        .server
        .handle(call(2, "reminders_write", delete))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "denied_by_permissions");
    assert_eq!(f.library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn without_an_approval_mode_deletions_ask_on_the_card_whatever_the_file_says() {
    let mut f = folder(None, true).await;
    let delete = json!({"action":"delete","guid":"R-1"});
    // No rule, allow, ask: client, and a broken file that would allow it: always the card,
    // never "Always allow", never a terminal request.
    for rule in [
        None,
        Some("rules:\n  reminders_write.delete: allow\n"),
        Some("rules:\n  reminders_write.delete: {ask: client}\n"),
        Some("rules:\n\treminders_write.delete: allow\n"),
    ] {
        match rule {
            Some(text) => write_rules(f.path(), text),
            None => {
                let _ = std::fs::remove_file(f.path().join(permissions::FILE_NAME));
            }
        }
        let form = f
            .server
            .handle(call(1, "reminders_write", delete.clone()))
            .await
            .unwrap();
        assert_eq!(form["method"], "elicitation/create", "{rule:?}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"], "{rule:?}");
        assert!(!form.to_string().contains("request_id"), "{rule:?}");
        f.server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
            .await;
        assert_eq!(f.library.outbox_count().unwrap(), 0);
    }
    assert!(!f.path().join(permissions::LOG_NAME).exists());
    // Approved on the card, it runs once.
    write_rules(f.path(), "rules:\n  reminders_write.delete: allow\n");
    let form = f
        .server
        .handle(call(1, "reminders_write", delete.clone()))
        .await
        .unwrap();
    let reply = f
        .server
        .handle(answer(&form, Some("approve")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    assert!(f
        .server
        .handle(answer(&form, Some("approve")))
        .await
        .is_none());
    assert_eq!(f.library.outbox_count().unwrap(), 1);
    // Routine changes still follow the rule.
    write_rules(f.path(), "rules:\n  reminders_write.create: allow\n");
    let reply = f
        .server
        .handle(call(2, "reminders_write", create_reminder()))
        .await
        .unwrap();
    assert_eq!(body(&reply)["status"], "queued");
    // A client without cards gets a refusal, and no terminal request is made.
    let mut f = folder(None, false).await;
    let reply = f
        .server
        .handle(call(1, "reminders_write", delete))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["error"], "approval_requires_form_client", "{reply}");
    assert!(result.get("request_id").is_none(), "{result}");
    assert_eq!(f.library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn terminal_requests_go_stale_when_the_file_changes_and_older_ones_respect_deny() {
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
    write_rules(
        f.path(),
        "# edited\nrules:\n  reminders_write.create: {ask: terminal}\n",
    );
    assert_eq!(
        f.server.approve_request(&id).await,
        Err("approval_policy_changed")
    );
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    // A request from the ask: terminal opt-in, in a folder without an approval mode.
    let mut f = folder(None, false).await;
    let opt_in = "rules:\n  reminders_write.delete: {ask: terminal}\n";
    write_rules(f.path(), opt_in);
    let reply = f
        .server
        .handle(call(
            1,
            "reminders_write",
            json!({"action":"delete","guid":"R-1"}),
        ))
        .await
        .unwrap();
    let id = body(&reply)["request_id"].as_str().unwrap().to_string();
    write_rules(f.path(), "rules:\n  reminders_write.delete: deny\n");
    assert_eq!(
        f.server.approve_request(&id).await,
        Err("denied_by_permissions")
    );
    assert_eq!(f.server.approve(&id), Err("denied_by_permissions"));
    assert_eq!(f.library.outbox_count().unwrap(), 0);
    // The same file as when it was asked: it counts again, once.
    write_rules(f.path(), opt_in);
    assert_eq!(
        f.server.approve_request(&id).await.unwrap()["status"],
        "queued"
    );
}

#[tokio::test]
async fn status_reads_without_writing_and_counts_rules_in_plain_words() {
    let mut f = folder(Some("writes_only"), true).await;
    let dir = f.path().canonicalize().unwrap();
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: {ask: terminal}\n",
    );
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    permissions::show(&dir, true).unwrap();
    let recorded = f.library.state("mcp.permissions.loaded").unwrap();
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    assert!(
        lines.iter().any(|(t, _)| t
            == "Permissions file: 1 rule in use. Everything else follows the approval mode."),
        "{lines:?}"
    );
    // Two looser edits the server hasn't loaded: reported, newest first, nothing written.
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: {ask: terminal}\n  reminders_write.create: allow\n",
    );
    f.server.handle(call(2, "reminders_lists", json!({}))).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: allow\n  reminders_write.create: allow\n",
    );
    let recorded = f
        .library
        .state("mcp.permissions.loaded")
        .unwrap()
        .filter(|r| Some(r) != recorded.as_ref())
        .unwrap();
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let notice = &lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .unwrap()
        .0;
    assert!(
        notice.contains("most recently reminders_write.delete:"),
        "{notice}"
    );
    assert!(notice.contains("(and 1 earlier)"), "{notice}");
    assert_eq!(
        f.library.state("mcp.permissions.loaded").unwrap(),
        Some(recorded)
    );
}

#[tokio::test]
async fn permissions_show_in_a_folder_that_never_had_a_file_reports_no_change() {
    let mut f = folder(Some("writes_only"), true).await;
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    let shown = permissions::show(&f.path().canonicalize().unwrap(), true).unwrap();
    assert!(shown.contains("There is no permissions file."), "{shown}");
    assert!(
        shown.contains("No changes since you last looked."),
        "{shown}"
    );
    assert!(
        !shown.contains("removed") && !shown.contains("Last change loaded"),
        "{shown}"
    );
    // A folder that isn't set up gets a pointer to setup, and nothing is created.
    let empty = tempfile::tempdir().unwrap();
    std::fs::set_permissions(empty.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        permissions::show(empty.path(), false),
        Err("permissions_setup_required")
    );
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
fn commands_name_the_folder_after_permissions_show() {
    let dir = Path::new("/srv/sky");
    assert_eq!(
        skykeep_mcp::status::with_dir("Run skykeep-mcp permissions show to see it.", dir),
        "Run skykeep-mcp permissions show --state-dir /srv/sky to see it."
    );
}

#[tokio::test]
async fn a_looser_change_is_reported_until_the_person_looks() {
    let mut f = folder(Some("writes_only"), true).await;
    let dir = f.path().canonicalize().unwrap();
    write_rules(
        f.path(),
        "rules:\n  reminders_write.create: {ask: terminal}\n",
    );
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    permissions::show(&dir, true).unwrap();
    assert!(permissions::status(&f.library, &dir, Some("writes_only"))
        .iter()
        .all(|(_, next)| next.is_none()));
    write_rules(f.path(), "rules:\n  reminders_write.create: allow\n");
    f.server.handle(call(2, "reminders_lists", json!({}))).await;
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let looser = lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .expect("reported");
    assert!(looser.0.contains("reminders_write.create: waits for skykeep-mcp approve in your terminal before, now runs without asking"), "{}", looser.0);
    assert!(looser.1.as_deref().unwrap().contains("permissions show"));
    let shown = permissions::show(&dir, true).unwrap();
    assert!(shown.contains("looser: reminders_write.create"), "{shown}");
    assert!(shown.contains("Last change loaded: "), "{shown}");
    assert!(permissions::status(&f.library, &dir, Some("writes_only"))
        .iter()
        .all(|(t, _)| !t.contains("became looser")));
    // Removing the file is a change too.
    std::fs::remove_file(f.path().join(permissions::FILE_NAME)).unwrap();
    let shown = permissions::show(&dir, false).unwrap();
    assert!(shown.contains("the file was removed"), "{shown}");
}

/// An allow rule never lets a deletion run unasked, in any mode: a Notes or Reminders delete
/// still shows the card with approve and deny only, runs nothing until it is approved, and is
/// never written to the log as having run without asking.
#[tokio::test]
async fn an_allowed_deletion_still_asks_on_the_card_and_is_never_logged() {
    for mode in [Some("writes_only"), Some("yolo"), None] {
        let mut f = folder(mode, true).await;
        write_rules(
            f.path(),
            "rules:\n  notes_write.create: allow\n  notes_write.delete: allow\n  reminders_write.delete: allow\n",
        );
        let reply = f
            .server
            .handle(call(
                1,
                "notes_write",
                json!({"action":"create","body":"Synthetic note"}),
            ))
            .await
            .unwrap();
        let guid = body(&reply)["guid"].as_str().unwrap().to_string();
        let start = f.library.outbox_count().unwrap();
        for (n, (tool, args)) in [
            (
                "notes_write",
                json!({"action":"delete","guid":guid,"base_etag":null}),
            ),
            ("reminders_write", json!({"action":"delete","guid":"R-1"})),
        ]
        .into_iter()
        .enumerate()
        {
            let form = f.server.handle(call(2, tool, args)).await.unwrap();
            assert_eq!(
                form["method"], "elicitation/create",
                "{mode:?} {tool}: {form}"
            );
            assert_eq!(choices(&form), ["approve", "deny"], "{mode:?} {tool}");
            assert_eq!(
                f.library.outbox_count().unwrap(),
                start + n,
                "{mode:?} {tool}"
            );
            let reply = f
                .server
                .handle(answer(&form, Some("approve")))
                .await
                .unwrap();
            assert_eq!(body(&reply)["status"], "queued", "{mode:?} {tool}: {reply}");
            assert_eq!(f.library.outbox_count().unwrap(), start + n + 1);
        }
        assert!(!f.path().join(permissions::LOG_NAME).exists(), "{mode:?}");
        let shown = permissions::show(&f.path().canonicalize().unwrap(), false).unwrap();
        assert!(
            shown.contains("reminders_write.delete: allow, but asks in your MCP client, because a deletion always needs a person"),
            "{shown}"
        );
        assert!(!shown.contains("ran without asking:"), "{shown}");
    }
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
        .contains("Always allow: every Reminders create runs without asking from now on."));
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
    // Every deletion asks on the card, whatever the mode; ask: terminal is only described.
    {
        let ask = Effect::AskClient;
        let text = permissions::starter(Path::new("/srv/sky"));
        assert!(!text.contains("\n    ask: terminal"), "{text}");
        assert!(text.contains("ask: terminal   opt-in"), "{text}");
        assert!(
            text.contains(
                "# To check it, run: skykeep-mcp permissions show --state-dir /srv/sky\n"
            ),
            "{text}"
        );
        let rules = permissions::parse(&text).unwrap();
        assert!(
            text.contains("keeps the last valid rules it loaded"),
            "{text}"
        );
        assert!(
            text.contains("ask on the approval card in your MCP client"),
            "{text}"
        );
        assert!(!text.contains("ignores all of it"), "{text}");
        assert_eq!(rules["notes_write.delete"], ask);
        assert_eq!(rules["reminders_write.delete"], ask);
        assert!(
            rules.keys().all(|k| {
                let (tool, action) = k.split_once('.').unwrap();
                permissions::is_deletion(tool, action)
            }),
            "{rules:?}"
        );
        assert_eq!(rules["drive_write.trash"], ask);
        assert_eq!(rules["drive_write.delete_forever"], ask);
        for name in permissions::names() {
            assert!(text.contains(&format!("#   {name}\n")), "{name}");
        }
    }
}

#[tokio::test]
async fn a_notes_move_into_recently_deleted_never_offers_always_allow() {
    let mut f = folder(Some("yolo"), true).await;
    let reply = f
        .server
        .handle(call(
            1,
            "notes_write",
            json!({"action":"create","body":"Synthetic note"}),
        ))
        .await
        .unwrap();
    let guid = body(&reply)["guid"].as_str().unwrap().to_string();
    write_rules(f.path(), "rules:\n  notes_write.move: {ask: client}\n");
    let trash = json!({"action":"move","guid":guid,"folder_guid":quince_core::notes::TRASH_FOLDER,"base_etag":null});
    let form = f
        .server
        .handle(call(2, "notes_write", trash))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny"]);
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "approval_denied");
    let saved = std::fs::read_to_string(f.path().join(permissions::FILE_NAME)).unwrap();
    assert!(!saved.contains("notes_write.move: allow"), "{saved}");
    // Another Notes change still offers it.
    let other = json!({"action":"pin","guid":guid,"pinned":true,"base_etag":null});
    write_rules(f.path(), "rules:\n  notes_write.pin: {ask: client}\n");
    let form = f
        .server
        .handle(call(3, "notes_write", other))
        .await
        .unwrap();
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
}

#[tokio::test]
async fn permissions_show_describes_deletions_as_the_server_runs_them() {
    let mut f = folder(None, true).await;
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: allow\n  notes_write: allow\n  reminders_write.create: allow\n",
    );
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    let shown = permissions::show(&f.path().canonicalize().unwrap(), false).unwrap();
    assert!(shown.contains("reminders_write.delete: allow, but asks in your MCP client, because a deletion always needs a person"), "{shown}");
    assert!(!shown.contains("skykeep-mcp approve"), "{shown}");
    assert!(
        shown
            .contains("notes_write: allow, runs without asking, except deletions, which still ask"),
        "{shown}"
    );
    assert!(
        shown.contains("reminders_write.create: allow, runs without asking\n"),
        "{shown}"
    );
}

#[tokio::test]
async fn the_looser_notice_survives_many_later_edits() {
    let mut f = folder(Some("writes_only"), true).await;
    let dir = f.path().canonicalize().unwrap();
    write_rules(
        f.path(),
        "rules:\n  reminders_write.delete: {ask: terminal}\n",
    );
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    permissions::show(&dir, true).unwrap();
    write_rules(f.path(), "rules:\n  reminders_write.delete: allow\n");
    f.server.handle(call(2, "reminders_lists", json!({}))).await;
    for n in 0..60 {
        write_rules(
            f.path(),
            &format!("# edit {n}\nrules:\n  reminders_write.delete: allow\n"),
        );
        f.server.handle(call(3, "reminders_lists", json!({}))).await;
    }
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let notice = &lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .expect("still reported")
        .0;
    assert!(notice.contains("reminders_write.delete"), "{notice}");
    let shown = permissions::show(&dir, true).unwrap();
    assert!(
        shown.contains(
            "older than the history kept here; the newest looser change: reminders_write.delete"
        ),
        "{shown}"
    );
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    assert!(
        lines.iter().all(|(t, _)| !t.contains("became looser")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_pipe_in_place_of_the_log_never_hangs() {
    let mut f = folder(Some("writes_only"), true).await;
    let log = f.path().join(permissions::LOG_NAME);
    let c = std::ffi::CString::new(log.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    write_rules(f.path(), "rules:\n  reminders_write.delete: allow\n");
    let dir = f.path().canonicalize().unwrap();
    let (done, wait) = std::sync::mpsc::channel();
    let shown_dir = dir.clone();
    std::thread::spawn(move || {
        let _ = done.send(permissions::show(&shown_dir, false));
    });
    let shown = wait
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("permissions show must not wait on a pipe")
        .unwrap();
    assert!(
        shown.contains("No deletion has run without asking"),
        "{shown}"
    );
    let delete = call(
        1,
        "reminders_write",
        json!({"action":"delete","guid":"R-1"}),
    );
    let reply = tokio::time::timeout(std::time::Duration::from_secs(10), f.server.handle(delete))
        .await
        .expect("a deletion must not wait on a pipe")
        .unwrap();
    // The deletion asks on the card; approved, it runs and writes nothing to the log.
    assert_eq!(reply["method"], "elicitation/create", "{reply}");
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.server.handle(answer(&reply, Some("approve"))),
    )
    .await
    .expect("a deletion must not wait on a pipe")
    .unwrap();
    let result = body(&reply);
    assert_eq!(result["status"], "queued", "{reply}");
    assert!(result.get("permissions_log").is_none(), "{result}");
}

#[tokio::test]
async fn an_unknown_name_points_to_the_name_list_outside_permissions_show_only() {
    let mut f = folder(Some("writes_only"), true).await;
    write_rules(f.path(), "rules:\n  notes_write.explode: allow\n");
    let reply = f
        .server
        .handle(call(1, "reminders_lists", json!({})))
        .await
        .unwrap();
    assert!(body(&reply)["permissions_file"]
        .as_str()
        .unwrap()
        .contains("To see every valid name, run skykeep-mcp permissions show."));
    let shown = permissions::show(&f.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        !shown.contains("run skykeep-mcp permissions show"),
        "{shown}"
    );
    assert!(shown.contains("Valid names: "), "{shown}");
}

#[tokio::test]
async fn a_deny_names_the_rule_that_refused_it() {
    let mut f = folder(Some("writes_only"), true).await;
    for (rules, args, rule) in [
        (
            "rules:\n  reminders_write: deny\n",
            create_reminder(),
            "reminders_write",
        ),
        (
            "rules:\n  reminders_write.create: deny\n",
            create_reminder(),
            "reminders_write.create",
        ),
        (
            "rules:\n  notes_write.delete: deny\n",
            json!({"action":"save","guid":"N","body":""}),
            "notes_write.delete",
        ),
    ] {
        write_rules(f.path(), rules);
        let tool = if rule.starts_with("notes") {
            "notes_write"
        } else {
            "reminders_write"
        };
        let reply = f.server.handle(call(1, tool, args)).await.unwrap();
        let result = body(&reply);
        assert_eq!(result["error"], "denied_by_permissions");
        assert_eq!(result["rule"], rule, "{result}");
        assert_eq!(
            result["message"],
            format!("The rule {rule}: deny in the permissions file refuses this action, so it did not run.")
        );
    }
}

#[tokio::test]
async fn a_shared_side_file_is_named_with_the_fix_in_the_tool_result_and_status() {
    let mut f = folder(Some("writes_only"), true).await;
    let side = f.path().join("permissions.yaml~");
    std::fs::write(&side, "rules:\n").unwrap();
    std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o664)).unwrap();
    let reply = f
        .server
        .handle(call(1, "reminders_lists", json!({})))
        .await
        .unwrap();
    let result = body(&reply);
    assert_eq!(result["error"], "unsafe_state_contents");
    let text = result["instructions"].as_str().unwrap();
    let shown = side.display().to_string();
    // A name with ~ is quoted for the shell.
    let quoted = format!("'{shown}'");
    assert!(
        text.contains(&format!("others can read or change {quoted}")),
        "{text}"
    );
    assert!(text.contains(&format!("chmod 600 {quoted}")), "{text}");
    assert!(
        text.contains("Delete it if it is an editor's backup copy"),
        "{text}"
    );
    let report = skykeep_mcp::status::check(skykeep_mcp::storage::env(), f.path());
    let rendered = report.render();
    assert!(
        rendered.contains(&format!("Others can read or change {shown}.")),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("chmod 600 {quoted}")),
        "{rendered}"
    );
}

/// The terminal stays as an opt-in: an ask: terminal rule, for a change or a deletion, returns
/// a request ID in every mode, and skykeep-mcp approve runs it once.
#[tokio::test]
async fn an_ask_terminal_rule_still_uses_the_terminal_in_every_mode() {
    for mode in [
        Some("reads_and_writes"),
        Some("writes_only"),
        Some("yolo"),
        None,
    ] {
        let mut f = folder(mode, true).await;
        write_rules(
            f.path(),
            "rules:\n  reminders_write.create: {ask: terminal}\n  reminders_write.delete:\n    ask: terminal\n",
        );
        for (n, args) in [create_reminder(), json!({"action":"delete","guid":"R-1"})]
            .into_iter()
            .enumerate()
        {
            let reply = f
                .server
                .handle(call(1, "reminders_write", args.clone()))
                .await
                .unwrap();
            assert_ne!(reply["method"], "elicitation/create", "{mode:?}: {reply}");
            let result = body(&reply);
            assert_eq!(result["status"], "approval_required", "{mode:?}: {reply}");
            let id = result["request_id"].as_str().unwrap().to_string();
            assert_eq!(f.library.outbox_count().unwrap(), n, "{mode:?}");
            let approved = f.server.approve_request(&id).await.unwrap();
            assert_eq!(approved["status"], "queued", "{mode:?}: {args}");
            assert!(f.server.approve_request(&id).await.is_err(), "used once");
            assert_eq!(f.library.outbox_count().unwrap(), n + 1, "{mode:?}");
        }
    }
}

/// A Notes delete moves the note to Recently Deleted, yet as a deletion its card offers no
/// "Always allow", and an answer that tries it anyway counts as Deny.
#[tokio::test]
async fn a_notes_delete_card_never_offers_always_allow() {
    let mut f = folder(Some("writes_only"), true).await;
    write_rules(f.path(), "rules:\n  notes_write.create: allow\n");
    let reply = f
        .server
        .handle(call(
            1,
            "notes_write",
            json!({"action":"create","body":"Synthetic note"}),
        ))
        .await
        .unwrap();
    let guid = body(&reply)["guid"].as_str().unwrap().to_string();
    let before = f.library.outbox_count().unwrap();
    let form = f
        .server
        .handle(call(
            2,
            "notes_write",
            json!({"action":"delete","guid":guid,"base_etag":null}),
        ))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny"]);
    let reply = f
        .server
        .handle(answer(&form, Some("always_allow")))
        .await
        .unwrap();
    assert_eq!(body(&reply)["error"], "approval_denied");
    assert_eq!(f.library.outbox_count().unwrap(), before);
    let saved = std::fs::read_to_string(f.path().join(permissions::FILE_NAME)).unwrap();
    assert!(!saved.contains("notes_write.delete"), "{saved}");
}
