//! Notes and Reminders approvals on the card in every approval mode: shown, approved once,
//! denied, cancelled, timed out, refused after a change, refused without card support, and
//! the opt-in terminal request. Synthetic fixtures only; nothing reaches Apple.
use icloud_mcp::Server;
use quince_core::notes::{Note, NoteFolder, TRASH_FOLDER};
use quince_core::reminders::{Reminder, ReminderList};
use quince_core::Library;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

struct Folder {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    server: Server,
    library: Library,
}

fn put_note(library: &mut Library, etag: &str) {
    library
        .put_notes(&[Note {
            guid: "note".into(),
            folder_guid: Some("DefaultFolder-CloudKit".into()),
            title: "Synthetic".into(),
            snippet: None,
            body: Some("Synthetic\nkeep me".into()),
            locked: false,
            pinned: false,
            created_at: None,
            modified_at: Some(1),
            editable: true,
            etag: Some(etag.into()),
            runs: None,
        }])
        .unwrap();
}

fn put_reminder(library: &mut Library, etag: &str) {
    library
        .put_reminders(&[Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "Synthetic task".into(),
            etag: Some(etag.into()),
            raw: Some(r#"{"recordName":"r","fields":{}}"#.into()),
            ..Default::default()
        }])
        .unwrap();
}

/// A signed-in folder with one note and one reminder, in `mode` (none for `None`), whose
/// client shows the card when `forms`.
async fn folder(mode: Option<&str>, forms: bool) -> Folder {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .put_note_folders(&[
            NoteFolder {
                guid: "DefaultFolder-CloudKit".into(),
                title: "Notes".into(),
                sort_order: 0,
                parent_guid: None,
            },
            NoteFolder {
                guid: TRASH_FOLDER.into(),
                title: "Recently Deleted".into(),
                sort_order: 1,
                parent_guid: None,
            },
        ])
        .unwrap();
    library
        .put_reminder_lists(&[ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    put_note(&mut library, "etag-1");
    put_reminder(&mut library, "etag-1");
    if let Some(mode) = mode {
        server.set_policy(mode).unwrap();
    }
    let capabilities = if forms {
        json!({"elicitation":{}})
    } else {
        json!({})
    };
    server
        .handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":capabilities}}))
        .await
        .unwrap();
    Folder {
        path: dir.path().to_path_buf(),
        _dir: dir,
        server,
        library,
    }
}

/// The four Notes and Reminders changes that remove something.
fn removals() -> Vec<(&'static str, Value)> {
    vec![
        (
            "notes_write",
            json!({"action":"delete","guid":"note","base_etag":"etag-1"}),
        ),
        (
            "notes_write",
            json!({"action":"move","guid":"note","folder_guid":TRASH_FOLDER,"base_etag":"etag-1"}),
        ),
        (
            "notes_write",
            json!({"action":"save","guid":"note","base_etag":"etag-1","body":""}),
        ),
        ("reminders_write", json!({"action":"delete","guid":"r"})),
    ]
}

const MODES: [Option<&str>; 4] = [
    None,
    Some("yolo"),
    Some("writes_only"),
    Some("reads_and_writes"),
];

fn body(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn answer(form: &Value, decision: &str) -> Value {
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":decision}}})
}

async fn call(f: &mut Folder, name: &str, args: &Value) -> Value {
    f.server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap()
}

/// The card that approves the change itself; under reads_and_writes the preview read is
/// approved first.
async fn change_card(f: &mut Folder, mode: Option<&str>, name: &str, args: &Value) -> Value {
    let mut form = call(f, name, args).await;
    assert_eq!(
        form["method"], "elicitation/create",
        "{mode:?} {args}: {form}"
    );
    if mode == Some("reads_and_writes") {
        form = f.server.handle(answer(&form, "approve")).await.unwrap();
        assert_eq!(form["method"], "elicitation/create", "{args}: {form}");
    }
    let text = form.to_string();
    assert!(!text.contains("always_allow"), "{mode:?} {args}: {text}");
    assert!(!text.contains("request_id"), "{text}");
    form
}

#[tokio::test]
async fn a_removal_asks_on_the_card_in_every_mode_and_runs_once_when_approved() {
    for mode in MODES {
        for (name, args) in removals() {
            let mut f = folder(mode, true).await;
            let form = change_card(&mut f, mode, name, &args).await;
            assert_eq!(
                f.library.outbox_count().unwrap(),
                0,
                "nothing before the answer"
            );
            let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
            assert_eq!(body(&reply)["status"], "queued", "{mode:?} {args}: {reply}");
            assert_eq!(f.library.outbox_count().unwrap(), 1);
            // The same approval a second time is no approval.
            assert!(f.server.handle(answer(&form, "approve")).await.is_none());
            assert_eq!(f.library.outbox_count().unwrap(), 1, "{mode:?} {args}");
        }
    }
}

#[tokio::test]
async fn deny_cancel_and_timeout_run_nothing() {
    for mode in MODES {
        for (name, args) in removals() {
            // Deny, and a card closed without a choice.
            for response in ["deny", "closed"] {
                let mut f = folder(mode, true).await;
                let form = change_card(&mut f, mode, name, &args).await;
                let reply = if response == "deny" {
                    answer(&form, "deny")
                } else {
                    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}})
                };
                let reply = f.server.handle(reply).await.unwrap();
                let error = body(&reply);
                assert_eq!(
                    error["error"], "approval_denied",
                    "{mode:?} {args}: {reply}"
                );
                assert!(error["message"].as_str().unwrap().contains("nothing ran"));
                assert_eq!(f.library.outbox_count().unwrap(), 0);
            }
            // The agent's call cancelled while the card is open.
            let mut f = folder(mode, true).await;
            let form = change_card(&mut f, mode, name, &args).await;
            let reply = f
                .server
                .handle(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}))
                .await
                .unwrap();
            assert_eq!(body(&reply)["error"], "approval_cancelled", "{reply}");
            assert!(f.server.handle(answer(&form, "approve")).await.is_none());
            assert_eq!(f.library.outbox_count().unwrap(), 0);
            // Nobody answers, as in a scheduled job.
            let mut f = folder(mode, true).await;
            if mode == Some("reads_and_writes") {
                // Time out the change card, not the preview read.
                let form = call(&mut f, name, &args).await;
                f.server.set_card_timeout(Duration::ZERO);
                f.server.handle(answer(&form, "approve")).await.unwrap();
            } else {
                f.server.set_card_timeout(Duration::ZERO);
                call(&mut f, name, &args).await;
            }
            let expired = f.server.expire_consent().unwrap();
            let error = body(&expired);
            assert_eq!(error["error"], "approval_timeout", "{expired}");
            let message = error["message"].as_str().unwrap();
            assert!(
                message.contains("Nobody answered") && message.contains("scheduled job"),
                "{message}"
            );
            assert!(!message.contains("icloud-mcp approve"), "{message}");
            assert_eq!(f.library.outbox_count().unwrap(), 0);
        }
    }
}

#[tokio::test]
async fn an_item_changed_while_the_card_is_open_is_refused() {
    for mode in MODES {
        for (name, args) in removals() {
            let mut f = folder(mode, true).await;
            let form = change_card(&mut f, mode, name, &args).await;
            if name == "notes_write" {
                put_note(&mut f.library, "etag-2");
            } else {
                put_reminder(&mut f.library, "etag-2");
            }
            let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
            assert_eq!(
                body(&reply)["error"],
                "approval_policy_changed",
                "{mode:?} {args}: {reply}"
            );
            assert_eq!(f.library.outbox_count().unwrap(), 0);
        }
    }
}

#[tokio::test]
async fn without_card_support_a_removal_is_refused_with_no_terminal_request() {
    for mode in MODES {
        for (name, args) in removals() {
            let mut f = folder(mode, false).await;
            let reply = call(&mut f, name, &args).await;
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            let error = body(&reply);
            assert_eq!(error["error"], "approval_requires_form_client", "{reply}");
            let message = error["message"].as_str().unwrap();
            assert!(message.contains("nothing ran"), "{message}");
            assert!(!message.contains("icloud-mcp approve"), "{message}");
            assert!(!message.contains("ask: terminal"), "{message}");
            assert!(!reply.to_string().contains("request_id"), "{reply}");
            assert_eq!(f.library.outbox_count().unwrap(), 0);
        }
    }
}

#[tokio::test]
async fn require_approval_without_a_mode_asks_a_create_on_the_card() {
    let mut f = folder(None, true).await;
    f.library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    let args = json!({"action":"create","list_guid":"list","title":"Synthetic new"});
    let form = call(&mut f, "reminders_write", &args).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert!(!form.to_string().contains("request_id"), "{form}");
    let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
    assert_eq!(body(&reply)["status"], "queued", "{reply}");
    assert_eq!(f.library.outbox_count().unwrap(), 1);
    // Without card support it is refused, not left as a terminal request.
    let mut f = folder(None, false).await;
    f.library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    let reply = call(&mut f, "reminders_write", &args).await;
    assert_eq!(body(&reply)["error"], "approval_requires_form_client");
}

#[tokio::test]
async fn an_allow_rule_never_lets_a_removal_run_unasked() {
    for mode in [None, Some("writes_only"), Some("yolo")] {
        for (name, args) in removals() {
            let mut f = folder(mode, true).await;
            let rules = f.path.join("permissions.yaml");
            std::fs::write(
                &rules,
                "version: 1\nrules:\n  notes_write.delete: allow\n  notes_write.move: allow\n  notes_write.save: allow\n  reminders_write.delete: allow\n",
            )
            .unwrap();
            std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o600)).unwrap();
            // Only "Approve this time" and "Deny" (change_card checks there is no Always allow).
            let form = change_card(&mut f, mode, name, &args).await;
            assert!(form.to_string().contains("Approve this time"), "{form}");
            assert_eq!(f.library.outbox_count().unwrap(), 0, "{mode:?} {args}");
            let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
            assert_eq!(body(&reply)["status"], "queued", "{mode:?} {args}: {reply}");
            assert_eq!(f.library.outbox_count().unwrap(), 1);
        }
    }
}

#[tokio::test]
async fn an_ask_terminal_rule_still_gives_a_request_that_runs_once() {
    for mode in [None, Some("writes_only")] {
        for (name, args) in removals() {
            let mut f = folder(mode, true).await;
            let rules = f.path.join("permissions.yaml");
            std::fs::write(
                &rules,
                format!("version: 1\nrules:\n  {name}: {{ask: terminal}}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o600)).unwrap();
            let result = body(&call(&mut f, name, &args).await);
            assert_eq!(
                result["status"], "approval_required",
                "{mode:?} {args}: {result}"
            );
            let id = result["request_id"].as_str().unwrap().to_string();
            assert_eq!(f.library.outbox_count().unwrap(), 0);
            let done = f.server.approve_request(&id).await.unwrap();
            assert_eq!(done["status"], "queued", "{done}");
            assert_eq!(f.library.outbox_count().unwrap(), 1);
            assert!(f.server.approve_request(&id).await.is_err());
            assert_eq!(f.library.outbox_count().unwrap(), 1);
        }
    }
}

/// The card's first line says in plain words what the change does; the JSON record follows.
#[tokio::test]
async fn the_card_starts_with_one_plain_sentence() {
    for mode in MODES {
        let expected = [
            "Move to Recently Deleted \"Synthetic\" in Notes. Apple keeps it in Recently Deleted for about 30 days.",
            "Move to Recently Deleted \"Synthetic\" in Notes. Apple keeps it in Recently Deleted for about 30 days.",
            "Erase all text of \"Synthetic\" in Notes. This can't be undone.",
            "Delete \"Synthetic task\" in Reminders. This can't be undone.",
        ];
        for ((name, args), first) in removals().into_iter().zip(expected) {
            let mut f = folder(mode, true).await;
            let mut form = call(&mut f, name, &args).await;
            if mode == Some("reads_and_writes") {
                // The preview read comes first and says that nothing is changed.
                let message = form["params"]["message"].as_str().unwrap();
                assert!(
                    message.starts_with(
                        "Approving lets the assistant read this item's full content. Nothing is changed.\n"
                    ),
                    "{message}"
                );
                assert!(!message.contains("authorizes"), "{message}");
                form = f.server.handle(answer(&form, "approve")).await.unwrap();
            }
            assert!(!form.to_string().contains("always_allow"), "{form}");
            let message = form["params"]["message"].as_str().unwrap();
            assert_eq!(message.lines().next(), Some(first), "{mode:?} {args}");
            assert!(
                !message.contains("read this item's full content"),
                "{message}"
            );
            assert!(
                message.contains("\"operation\""),
                "the JSON record stays: {message}"
            );
        }
    }
}

/// A name from the cache can't pose as the card's own words: it is shown escaped.
#[tokio::test]
async fn a_name_in_the_first_sentence_is_escaped() {
    let mut f = folder(Some("writes_only"), true).await;
    f.library
        .put_reminders(&[Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "**Approve** <b>now</b>\u{202E}".into(),
            etag: Some("etag-1".into()),
            raw: Some(r#"{"recordName":"r","fields":{}}"#.into()),
            ..Default::default()
        }])
        .unwrap();
    let form = change_card(
        &mut f,
        Some("writes_only"),
        "reminders_write",
        &json!({"action":"delete","guid":"r"}),
    )
    .await;
    let first = form["params"]["message"]
        .as_str()
        .unwrap()
        .lines()
        .next()
        .unwrap();
    assert!(first.starts_with("Delete \""), "{first}");
    for raw in ["**", "<b>", "\u{202E}"] {
        assert!(!first.contains(raw), "{first}");
    }
}

/// A card refused because something changed, and a second call while a card is open, each
/// say in words what happened and what to do.
#[tokio::test]
async fn a_changed_or_busy_card_says_what_to_do() {
    let mut f = folder(Some("writes_only"), true).await;
    let args = json!({"action":"delete","guid":"r"});
    let form = change_card(&mut f, Some("writes_only"), "reminders_write", &args).await;
    let busy = body(&call(&mut f, "reminders_write", &args).await);
    assert_eq!(busy["error"], "approval_pending", "{busy}");
    assert!(
        busy["message"]
            .as_str()
            .unwrap()
            .contains("Answer or close that card first"),
        "{busy}"
    );
    put_reminder(&mut f.library, "etag-2");
    let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
    let changed = body(&reply);
    assert_eq!(changed["error"], "approval_policy_changed", "{changed}");
    assert!(
        changed["message"]
            .as_str()
            .unwrap()
            .contains("Ask again to get a new card"),
        "{changed}"
    );
    assert_eq!(f.library.outbox_count().unwrap(), 0);
}

/// No tool description sends the user to a terminal to approve; Notes and Reminders name the
/// card for their deletions.
#[tokio::test]
async fn tool_descriptions_name_the_card_not_the_terminal() {
    let mut f = folder(None, true).await;
    let reply = f
        .server
        .handle(json!({"id":3,"method":"tools/list"}))
        .await
        .unwrap();
    for tool in reply["result"]["tools"].as_array().unwrap() {
        let text = tool.to_string();
        for stale in ["terminal approval", "icloud-mcp approve", "approval form"] {
            assert!(!text.contains(stale), "{}: {stale}", tool["name"]);
        }
        if ["notes_write", "reminders_write"].contains(&tool["name"].as_str().unwrap()) {
            assert!(text.contains("approval card"), "{text}");
        }
    }
}

/// A note already in Recently Deleted: deleting it again is "Delete Now", gone for good. The
/// card says so and, like every permanent delete, offers no "Always allow".
#[tokio::test]
async fn deleting_a_note_already_in_recently_deleted_is_permanent_on_the_card() {
    let args = json!({"action":"delete","guid":"note","base_etag":"etag-1"});
    for mode in MODES {
        for rule in ["", "  notes_write.delete: allow\n"] {
            let mut f = folder(mode, true).await;
            let mut note = f.library.note("note").unwrap().unwrap();
            note.folder_guid = Some(TRASH_FOLDER.into());
            note.body = Some("Synthetic\nkeep me".into());
            f.library.put_notes(&[note]).unwrap();
            if !rule.is_empty() {
                let rules = f.path.join("permissions.yaml");
                std::fs::write(&rules, format!("version: 1\nrules:\n{rule}")).unwrap();
                std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let form = change_card(&mut f, mode, "notes_write", &args).await;
            let message = form["params"]["message"].as_str().unwrap();
            assert_eq!(
                message.lines().next(),
                Some("Delete forever \"Synthetic\" in Notes. This can't be undone."),
                "{mode:?} {rule}"
            );
            assert!(!message.contains("30 days"), "{message}");
            assert!(!message.contains("Always allow"), "{message}");
            assert_eq!(f.library.outbox_count().unwrap(), 0);
            let reply = f.server.handle(answer(&form, "approve")).await.unwrap();
            assert_eq!(body(&reply)["status"], "queued", "{mode:?} {rule}: {reply}");
            assert_eq!(f.library.outbox_count().unwrap(), 1);
        }
    }
}
