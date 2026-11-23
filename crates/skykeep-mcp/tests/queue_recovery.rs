//! Security review 02 regressions. A stale or orphaned write must never stop the whole
//! queue, and refused rows must never use up write admission. Synthetic data only.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn saved_client(dir: &std::path::Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some("1234567890".into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

async fn remote() -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{"ckdatabasews":{"url":remote.uri()}}}))).mount(&remote).await;
    Mock::given(method("POST"))
        .and(path_regex(".*/changes/zone$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"zones":[{"records":[],"syncToken":"synthetic","moreComing":false}]}),
        ))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/records/lookup$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[]})))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/records/modify$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"records":[{"recordName":"synthetic"}]})),
        )
        .mount(&remote)
        .await;
    remote
}

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[tokio::test]
async fn policy_migration_restores_populated_remote_and_keeps_failed_reread_pending() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"UNSENT-EDIT"}}),
    )
    .await;
    let created = write(
        &mut server,
        3,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"UNSENT-CREATE"}),
    )
    .await;
    let created: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    let local_guid = created["guid"].as_str().unwrap();
    let kept = library
        .outbox_append("notes", "already-refused", "save", "unchanged history")
        .unwrap();
    library.outbox_refuse(kept, "already reported").unwrap();
    server.set_policy("yolo").unwrap();
    assert_eq!(
        library
            .state("mcp.full_reread.reminders")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert_eq!(library.state("mcp.full_reread.notes").unwrap(), None);
    assert!(library.outbox_pending("reminders").unwrap().is_empty());
    assert!(library
        .outbox_refused()
        .unwrap()
        .iter()
        .any(|r| r.change.id == kept
            && r.change.payload == "unchanged history"
            && r.reason == "already reported"));
    drop(server);
    let _reopened = skykeep_mcp::Server::open(dir.path()).unwrap();
    assert_eq!(
        library
            .state("mcp.full_reread.reminders")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    let remote = remote().await;
    let failed = Arc::new(AtomicBool::new(false));
    let fail_once = failed.clone();
    Mock::given(method("POST")).and(path_regex(".*/com.apple.reminders/.*/changes/zone$"))
        .respond_with(move |request: &wiremock::Request| {
            let delta = String::from_utf8_lossy(&request.body).contains("prior");
            if !delta && !fail_once.load(Ordering::SeqCst) { return ResponseTemplate::new(400); }
            let records = if delta { json!([]) } else { json!([
                {"recordName":"list","recordType":"List","recordChangeTag":"remote-list","fields":{"Name":{"type":"STRING","value":"Remote list"}}},
                {"recordName":"other","recordType":"Reminder","recordChangeTag":"remote-item","fields":{"List":{"type":"REFERENCE","value":{"recordName":"list"}},"TitleDocument":{"type":"ENCRYPTED_BYTES","value":quince_icloud::ckdocument::text_to_base64("UNCHANGED-REMOTE-TITLE")}}}
            ]) };
            ResponseTemplate::new(200).set_body_json(json!({"zones":[{"records":records,"syncToken":"restored","moreComing":false}]}))
        }).with_priority(1).mount(&remote).await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert!(
        remote.received_requests().await.unwrap().iter().any(|r| r
            .url
            .path()
            .contains("com.apple.reminders")
            && r.url.path().ends_with("/changes/zone")
            && !String::from_utf8_lossy(&r.body).contains("prior")),
        "the scheduled full read was attempted"
    );
    assert_eq!(
        library
            .state("mcp.full_reread.reminders")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        library.reminder("other").unwrap().unwrap().title,
        "UNSENT-EDIT"
    );
    failed.store(true, Ordering::SeqCst);
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(
        library.reminder("other").unwrap().unwrap().title,
        "UNCHANGED-REMOTE-TITLE"
    );
    assert!(library.reminder(local_guid).unwrap().is_none());
    assert_eq!(
        library
            .state("mcp.full_reread.reminders")
            .unwrap()
            .as_deref(),
        Some("")
    );
    assert!(
        library.note("note").unwrap().is_some(),
        "unaffected Notes cache remains incremental"
    );
    assert!(library
        .outbox_refused()
        .unwrap()
        .iter()
        .any(|r| r.change.id == kept && r.change.payload == "unchanged history"));
}

#[tokio::test]
async fn policy_migration_marks_both_affected_services_atomically() {
    let (_dir, mut server, library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"pending"}}),
    )
    .await;
    let result = write(
        &mut server,
        3,
        "notes_write",
        json!({"action":"pin","guid":"note","pinned":true,"base_etag":"n1"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let policy = library.state("mcp.policy").unwrap();
    library.conn().execute_batch("CREATE TRIGGER fail_reread BEFORE INSERT ON app_state WHEN NEW.key = 'mcp.full_reread.reminders' BEGIN SELECT RAISE(ABORT, 'synthetic rollback'); END;").unwrap();
    assert!(server.set_policy("writes_only").is_err());
    assert_eq!(library.state("mcp.policy").unwrap(), policy);
    assert_eq!(library.outbox_all_pending().unwrap().len(), 2);
    assert_eq!(library.state("mcp.full_reread.notes").unwrap(), None);
    library
        .conn()
        .execute_batch("DROP TRIGGER fail_reread")
        .unwrap();
    server.set_policy("writes_only").unwrap();
    for service in ["notes", "reminders"] {
        assert_eq!(
            library
                .state(&format!("mcp.full_reread.{service}"))
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }
}

#[tokio::test]
async fn policy_migration_restores_populated_remote_note_body() {
    let (dir, mut server, library) = family().await;
    let edited = write(
        &mut server,
        2,
        "notes_write",
        json!({"action":"save","guid":"note","base_etag":"n1","body":"UNSENT-NOTE-BODY"}),
    )
    .await;
    assert_eq!(edited["isError"], false);
    let created = write(
        &mut server,
        3,
        "notes_write",
        json!({"action":"create","body":"UNSENT-LOCAL-NOTE"}),
    )
    .await;
    let created: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    let local_guid = created["guid"].as_str().unwrap();
    server.set_policy("yolo").unwrap();
    assert_eq!(
        library.state("mcp.full_reread.notes").unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(library.state("mcp.full_reread.reminders").unwrap(), None);
    let remote = remote().await;
    Mock::given(method("POST")).and(path_regex(".*/com.apple.notes/.*/changes/zone$"))
        .respond_with(|request: &wiremock::Request| {
            let delta = String::from_utf8_lossy(&request.body).contains("prior");
            let records = if delta { json!([]) } else { json!([
                {"recordName":"note","recordType":"Note","recordChangeTag":"remote-note","fields":{"TitleEncrypted":{"type":"ENCRYPTED_BYTES","value":quince_icloud::ckdocument::text_to_base64("REMOTE-NOTE-TITLE")},"TextDataEncrypted":{"type":"ENCRYPTED_BYTES","value":quince_icloud::ckdocument::text_to_base64("REMOTE-NOTE-BODY")}}}
            ]) };
            ResponseTemplate::new(200).set_body_json(json!({"zones":[{"records":records,"syncToken":"restored-notes","moreComing":false}]}))
        }).with_priority(1).mount(&remote).await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(
        library.note("note").unwrap().unwrap().title,
        "REMOTE-NOTE-TITLE"
    );
    assert_eq!(
        library.note_body("note").unwrap().as_deref(),
        Some("REMOTE-NOTE-BODY")
    );
    assert!(library.note(local_guid).unwrap().is_none());
    assert_eq!(
        library.state("mcp.full_reread.notes").unwrap().as_deref(),
        Some("")
    );
    assert!(
        library.reminder("other").unwrap().is_some(),
        "unaffected service remains incremental"
    );
}

/// Two lists; "parent" has subtask "child"; "other" stands alone. Policy is yolo, so
/// every write is approved by the human-set policy without a form.
async fn family() -> (tempfile::TempDir, skykeep_mcp::Server, quince_core::Library) {
    let dir = private_dir();
    let mut server = skykeep_mcp::Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    library
        .put_reminder_lists(&[
            quince_core::reminders::ReminderList {
                guid: "list".into(),
                ctag: Some("c1".into()),
                ..Default::default()
            },
            quince_core::reminders::ReminderList {
                guid: "list2".into(),
                ..Default::default()
            },
        ])
        .unwrap();
    for (guid, parent) in [
        ("parent", None),
        ("child", Some("parent".to_string())),
        ("other", None),
    ] {
        library
            .put_reminder_local(&quince_core::reminders::Reminder {
                guid: guid.into(),
                list_guid: "list".into(),
                title: guid.into(),
                parent_guid: parent,
                ..Default::default()
            })
            .unwrap();
    }
    library
        .put_notes(&[quince_core::notes::Note {
            guid: "note".into(),
            title: "note".into(),
            folder_guid: None,
            snippet: None,
            body: None,
            locked: false,
            pinned: false,
            created_at: None,
            modified_at: None,
            runs: None,
            editable: true,
            etag: Some("n1".into()),
        }])
        .unwrap();
    library
        .set_note_text("note", "note\nbody", "note", Some("body"))
        .unwrap();
    // A prior full sync, so the synthetic pass is incremental and keeps these items.
    for service in ["notes", "reminders"] {
        library.mark_synced(service, Some("prior")).unwrap();
        library.mark_full_sync(service).unwrap();
    }
    server.set_policy("yolo").unwrap();
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
    (dir, server, library)
}

/// A call's result; a deletion's card (yolo still asks for those) is approved first.
async fn write(server: &mut skykeep_mcp::Server, id: i64, tool: &str, args: Value) -> Value {
    let mut reply = server
        .handle(json!({"id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}))
        .await
        .unwrap();
    if reply["method"] == "elicitation/create" {
        reply = server
            .handle(json!({"jsonrpc":"2.0","id":reply["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
            .await
            .unwrap();
    }
    reply["result"].clone()
}

async fn approved(server: &mut skykeep_mcp::Server, id: i64, args: Value) {
    let result = write(server, id, "reminders_write", args).await;
    assert_eq!(result["isError"], false, "{result}");
}

fn pending_for(library: &quince_core::Library, record: &str) -> quince_core::outbox::Pending {
    library
        .outbox_pending("reminders")
        .unwrap()
        .into_iter()
        .find(|p| p.record == record)
        .unwrap()
}

fn refused_reason(library: &quince_core::Library, record: &str) -> Option<String> {
    library
        .outbox_refused()
        .unwrap()
        .into_iter()
        .find(|r| r.change.record == record)
        .map(|r| r.reason)
}

fn modified(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.url.path().ends_with("/records/modify"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

/// The native sender looked up or wrote this record. The synthetic remote has no records,
/// so a tick may stop after its lookup; reaching the sender is what the gate controls.
fn handed_to_sender(requests: &[wiremock::Request], record: &str) -> bool {
    let quoted = format!("\"{record}\"");
    requests.iter().any(|r| {
        (r.url.path().ends_with("/records/modify") || r.url.path().ends_with("/records/lookup"))
            && String::from_utf8_lossy(&r.body).contains(&quoted)
    })
}

fn mcp_refusals(library: &quince_core::Library) -> Vec<String> {
    library
        .outbox_refused()
        .unwrap()
        .into_iter()
        .map(|r| r.reason)
        .filter(|r| r.starts_with("mcp_") || r.starts_with("superseded"))
        .collect()
}

#[tokio::test]
async fn approved_subtask_edit_keeps_parent_edit_sendable() {
    let (dir, mut server, library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"parent","patch":{"title":"approved parent edit"}}),
    )
    .await;
    approved(
        &mut server,
        3,
        json!({"action":"edit","guid":"child","patch":{"title":"approved child edit"}}),
    )
    .await;
    approved(
        &mut server,
        4,
        json!({"action":"tick","guid":"child","done":true}),
    )
    .await;
    for record in ["parent", "child"] {
        assert_eq!(
            skykeep_mcp::sync::authorized(&library, dir.path(), &pending_for(&library, record)),
            Ok(()),
            "{record}"
        );
    }
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(
        library.outbox_pending("reminders").unwrap().len(),
        0,
        "{result}"
    );
    assert_eq!(mcp_refusals(&library), Vec::<String>::new());
    let requests = remote.received_requests().await.unwrap();
    assert!(handed_to_sender(&requests, "parent"));
    assert!(handed_to_sender(&requests, "child"));
}

#[tokio::test]
async fn approved_list_move_rebinds_pending_subtask_and_parent_move_context() {
    let (dir, mut server, library) = family().await;
    // The subtask edit was approved first; the approved parent move then rewrites the
    // subtask's cached list. Then a later subtask write changes what the move reads.
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"child","patch":{"title":"approved child edit"}}),
    )
    .await;
    approved(
        &mut server,
        3,
        json!({"action":"edit","guid":"parent","patch":{"list_guid":"list2"}}),
    )
    .await;
    approved(
        &mut server,
        4,
        json!({"action":"tick","guid":"child","done":true}),
    )
    .await;
    assert_eq!(
        library.reminder("child").unwrap().unwrap().list_guid,
        "list2"
    );
    for p in library.outbox_pending("reminders").unwrap() {
        assert_eq!(
            skykeep_mcp::sync::authorized(&library, dir.path(), &p),
            Ok(()),
            "{p:?}"
        );
    }
}

#[tokio::test]
async fn approved_parent_deletion_supersedes_subtask_writes_without_blocking_sync() {
    let (dir, mut server, library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"child","patch":{"title":"approved edit"}}),
    )
    .await;
    approved(
        &mut server,
        3,
        json!({"action":"tick","guid":"other","done":true}),
    )
    .await;
    approved(&mut server, 4, json!({"action":"delete","guid":"parent"})).await;
    assert!(library.reminder("child").unwrap().is_none());
    assert_eq!(
        refused_reason(&library, "child").as_deref(),
        Some("superseded_by_approved_deletion")
    );
    let remote = remote().await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(library.outbox_pending("reminders").unwrap().len(), 0);
    let requests = remote.received_requests().await.unwrap();
    assert!(
        handed_to_sender(&requests, "other"),
        "unrelated tick reached the sender"
    );
    // The deletion's own payload takes the subtask along; the superseded edit never goes.
    assert!(!modified(&requests)
        .iter()
        .any(|b| b.contains("approved edit")));
}

#[tokio::test]
async fn cache_refresh_of_lists_does_not_stale_item_grants() {
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"approved"}}),
    )
    .await;
    let p = pending_for(&library, "other");
    let mut lists = vec![quince_core::reminders::ReminderList {
        guid: "list".into(),
        ctag: Some("c2".into()),
        ..Default::default()
    }];
    for i in 0..200 {
        lists.push(quince_core::reminders::ReminderList {
            guid: format!("l{i}"),
            title: "x".repeat(200),
            ..Default::default()
        });
    }
    library.put_reminder_lists(&lists).unwrap();
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, dir.path(), &p),
        Ok(())
    );
}

#[tokio::test]
async fn stale_write_is_quarantined_alone_and_never_sent() {
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"approved stale edit"}}),
    )
    .await;
    approved(
        &mut server,
        3,
        json!({"action":"tick","guid":"parent","done":true}),
    )
    .await;
    // Stand-in for another device changing the item after approval.
    let mut changed = library.reminder("other").unwrap().unwrap();
    changed.flagged = true;
    library.put_reminder_local(&changed).unwrap();
    let remote = remote().await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(
        refused_reason(&library, "other").as_deref(),
        Some("mcp_quarantine:item_changed_on_icloud_since_queued")
    );
    assert_eq!(library.outbox_pending("reminders").unwrap().len(), 0);
    let requests = remote.received_requests().await.unwrap();
    assert!(
        handed_to_sender(&requests, "parent"),
        "approved tick reached the sender"
    );
    assert!(!handed_to_sender(&requests, "other"));
    // The test helper's synthetic session file is not created user-only; fix that so the
    // server's private-state check accepts the directory again.
    use std::os::unix::fs::PermissionsExt;
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_file() {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    // The quarantine is visible in status; nothing on the tool path can release it.
    let status = write(&mut server, 9, "sync_status", json!({})).await;
    let status = status["content"][0]["text"].as_str().unwrap();
    assert!(
        status.contains("\"refused\":1"),
        "{status} {:?}",
        library
            .outbox_refused()
            .unwrap()
            .iter()
            .map(|r| (&r.change.record, &r.reason))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn unapproved_rows_are_quarantined_before_network_and_never_sent() {
    let (dir, _server, mut library) = family().await;
    let id = library
        .outbox_append("reminders", "unapproved", "create", "{}")
        .unwrap();
    let remote = remote().await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert!(modified(&remote.received_requests().await.unwrap()).is_empty());
    let refused = library.outbox_refused().unwrap();
    assert_eq!(refused[0].change.id, id);
    assert_eq!(refused[0].reason, "mcp_quarantine:legacy_write_quarantined");
}

#[tokio::test]
async fn refused_rows_do_not_use_up_write_admission() {
    let (_dir, mut server, library) = family().await;
    for id in 0..128 {
        approved(
            &mut server,
            100 + id,
            json!({"action":"create","list_guid":"list","title":format!("t{id}")}),
        )
        .await;
    }
    let over = write(
        &mut server,
        500,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"129th"}),
    )
    .await;
    assert_eq!(over["isError"], true);
    server.set_policy("yolo").unwrap();
    assert_eq!(library.outbox_refused().unwrap().len(), 128);
    approved(
        &mut server,
        501,
        json!({"action":"create","list_guid":"list","title":"after"}),
    )
    .await;
    let note = write(
        &mut server,
        502,
        "notes_write",
        json!({"action":"create","body":"x"}),
    )
    .await;
    assert_eq!(note["isError"], false, "{note}");
}

#[tokio::test]
async fn full_quarantine_refuses_admission_until_human_retires_it() {
    let (_dir, mut server, mut library) = family().await;
    let waiting = library
        .outbox_append("reminders", "waiting", "create", "{}")
        .unwrap();
    for i in 0..skykeep_mcp::sync::MAX_REFUSED {
        let id = library
            .outbox_append("reminders", &format!("r{i}"), "create", "{}")
            .unwrap();
        library.outbox_refuse(id, "synthetic").unwrap();
    }
    let full = write(
        &mut server,
        2,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"x"}),
    )
    .await;
    assert_eq!(full["isError"], true);
    assert!(full["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("quarantine_full_retire_in_terminal"));
    let shown = server.quarantined().unwrap();
    assert_eq!(shown.len(), skykeep_mcp::sync::MAX_REFUSED);
    assert!(shown.iter().all(|row| row.get("payload").is_none()));
    // A row refused after the human looked is not retired unseen; waiting rows never are.
    let late = library
        .outbox_append("reminders", "late", "create", "{}")
        .unwrap();
    library.outbox_refuse(late, "synthetic").unwrap();
    let ids: Vec<i64> = shown
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .chain([waiting])
        .collect();
    assert_eq!(
        server.retire_quarantined(&ids).unwrap(),
        skykeep_mcp::sync::MAX_REFUSED
    );
    let left = library.outbox_refused().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].change.id, late);
    assert_eq!(library.outbox_pending("reminders").unwrap()[0].id, waiting);
}

#[tokio::test]
async fn legacy_queue_larger_than_admission_cap_can_still_migrate() {
    let dir = private_dir();
    let mut server = skykeep_mcp::Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    for i in 0..129 {
        library
            .outbox_append("reminders", &format!("legacy{i}"), "create", "{}")
            .unwrap();
    }
    server.set_policy("writes_only").unwrap();
    assert_eq!(library.outbox_pending("reminders").unwrap().len(), 0);
    assert_eq!(library.outbox_refused().unwrap().len(), 129);
}

/// Security review 03, N1: an item that grows past the 128 KiB serializer limit after
/// approval must quarantine only its own write, not stop every later sync.
#[tokio::test]
async fn oversized_bound_items_are_quarantined_alone() {
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"tick","guid":"other","done":true}),
    )
    .await;
    approved(
        &mut server,
        3,
        json!({"action":"tick","guid":"parent","done":true}),
    )
    .await;
    let saved = write(
        &mut server,
        4,
        "notes_write",
        json!({"action":"save","guid":"note","body":"note\nbody edited","base_etag":"n1"}),
    )
    .await;
    assert_eq!(saved["isError"], false, "{saved}");
    let mut other = library.reminder("other").unwrap().unwrap();
    other.notes = Some("y".repeat(140_000));
    library.put_reminder_local(&other).unwrap();
    library
        .set_note_text(
            "note",
            &format!("note\n{}", "z".repeat(140_000)),
            "note",
            Some("z"),
        )
        .unwrap();
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(
        refused_reason(&library, "other").as_deref(),
        Some("mcp_quarantine:preview_too_large"),
        "{result}"
    );
    let note = library
        .outbox_refused()
        .unwrap()
        .into_iter()
        .find(|r| r.change.service == "notes")
        .unwrap();
    assert_eq!(note.reason, "mcp_quarantine:preview_too_large");
    let requests = remote.received_requests().await.unwrap();
    assert!(
        handed_to_sender(&requests, "parent"),
        "the approved tick still reached the sender"
    );
    assert!(!handed_to_sender(&requests, "other"));
}

/// Security review 03, N2: the sync reply names what this sync quarantined and counts
/// only waiting writes as queued; status says which write and why.
#[tokio::test]
async fn sync_reply_and_status_report_quarantined_writes() {
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"QUEUED-TITLE"}}),
    )
    .await;
    let mut changed = library.reminder("other").unwrap().unwrap();
    changed.priority = 5;
    library.put_reminder_local(&changed).unwrap();
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(result["queued"], 0, "{result}");
    assert_eq!(result["refused"], 1, "{result}");
    let quarantined = result["refused_this_sync"].as_array().unwrap();
    assert_eq!(quarantined.len(), 1, "{result}");
    assert_eq!(quarantined[0]["record"], "other");
    assert_eq!(quarantined[0]["kind"], "edit");
    assert_eq!(
        quarantined[0]["reason"],
        "mcp_quarantine:item_changed_on_icloud_since_queued"
    );
    assert!(!result.to_string().contains("QUEUED-TITLE"));
    use std::os::unix::fs::PermissionsExt;
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_file() {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let status = write(&mut server, 9, "sync_status", json!({})).await;
    let status: Value =
        serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    let reminders = status["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "reminders")
        .unwrap()
        .clone();
    assert_eq!(reminders["refused"], 1, "{status}");
    assert_eq!(
        reminders["refused_recent"][0]["record"], "other",
        "{status}"
    );
    assert_eq!(
        reminders["refused_recent"][0]["reason"],
        "mcp_quarantine:item_changed_on_icloud_since_queued"
    );
    assert!(!status.to_string().contains("QUEUED-TITLE"));
}

/// Security review 03, N2: a quarantined edit must not stay in the cache as if it were
/// on its way. After its drain the same sync re-reads the service from the start (no sync
/// token), and a full pass replays only waiting writes, so the unsent title is gone. The
/// empty synthetic remote makes that full pass drop the item entirely.
#[tokio::test]
async fn quarantined_edit_does_not_stay_in_the_cache() {
    let (dir, mut server, mut library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"QUEUED-TITLE"}}),
    )
    .await;
    assert_eq!(
        library.reminder("other").unwrap().unwrap().title,
        "QUEUED-TITLE"
    );
    let mut changed = library.reminder("other").unwrap().unwrap();
    changed.priority = 5;
    library.put_reminder_local(&changed).unwrap();
    let remote = remote().await;
    skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    let requests = remote.received_requests().await.unwrap();
    let reminder_reads: Vec<String> = requests
        .iter()
        .filter(|r| {
            r.url.path().contains("com.apple.reminders") && r.url.path().ends_with("/changes/zone")
        })
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert_eq!(reminder_reads.len(), 2, "{reminder_reads:?}");
    assert!(
        reminder_reads[0].contains("prior"),
        "first read is the usual delta"
    );
    assert!(!reminder_reads[1].contains("prior"), "then a full re-read");
    assert!(library
        .reminder("other")
        .unwrap()
        .is_none_or(|r| r.title != "QUEUED-TITLE"));
}

/// Security review 03, N3: the terminal listing drops text-direction characters, and
/// status shows engine reasons only as a code.
#[tokio::test]
async fn refused_reasons_are_plain_in_terminal_and_coded_in_status() {
    let (_dir, mut server, mut library) = family().await;
    let id = library
        .outbox_append("reminders", "rec\u{202E}ord", "edit", "{}")
        .unwrap();
    library
        .outbox_refuse(id, "HTTP 400 body\u{202E}\u{2066} SYNTHETIC-UPSTREAM-TEXT")
        .unwrap();
    let shown = server.quarantined().unwrap();
    assert_eq!(shown[0]["record"], "record");
    assert_eq!(shown[0]["reason"], "HTTP 400 body SYNTHETIC-UPSTREAM-TEXT");
    let status = write(&mut server, 9, "sync_status", json!({})).await;
    let text = status["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("refused_by_sync_engine"), "{text}");
    assert!(!text.contains("SYNTHETIC-UPSTREAM-TEXT"));
}

fn grant(library: &quince_core::Library, id: i64) -> Option<String> {
    library.state(&format!("mcp.authorization.{id}")).unwrap()
}

/// A synthetic remote whose Reminders read brings "other" back with a new etag, in `list`.
async fn remote_with_other_in(list: &str) -> MockServer {
    let remote = remote().await;
    let record = json!({"recordName":"other","recordType":"Reminder","recordChangeTag":"apple-2",
        "fields":{"List":{"type":"REFERENCE","value":{"recordName":list}},
        "TitleDocument":{"type":"ENCRYPTED_BYTES","value":quince_icloud::ckdocument::text_to_base64("other")}}});
    Mock::given(method("POST"))
        .and(path_regex(".*/com.apple.reminders/.*/changes/zone$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"zones":[{"records":[record],"syncToken":"next","moreComing":false}]}),
        ))
        .with_priority(1)
        .mount(&remote)
        .await;
    remote
}

/// Issue 74: Apple gives a new etag and time to a note or reminder at the next read, even
/// one just made. A write queued before that read still goes to the sender, which checks
/// the etag itself.
#[tokio::test]
async fn a_new_etag_and_time_from_apple_do_not_stale_a_queued_write() {
    let (dir, mut server, mut library) = family().await;
    let created = write(
        &mut server,
        2,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"fresh"}),
    )
    .await;
    let created: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    let fresh = created["guid"].as_str().unwrap().to_string();
    approved(
        &mut server,
        3,
        json!({"action":"tick","guid":fresh,"done":true}),
    )
    .await;
    let pinned = write(
        &mut server,
        4,
        "notes_write",
        json!({"action":"pin","guid":"note","pinned":true,"base_etag":"n1"}),
    )
    .await;
    assert_eq!(pinned["isError"], false, "{pinned}");
    // What the next read from Apple brings: new etags and a new time, nothing else.
    let mut reminder = library.reminder(&fresh).unwrap().unwrap();
    reminder.etag = Some("apple-1".into());
    library.put_reminders(&[reminder]).unwrap();
    let mut note = library.note("note").unwrap().unwrap();
    note.etag = Some("n2".into());
    note.modified_at = Some(1_700_000_000);
    library.put_notes(&[note]).unwrap();
    for service in ["notes", "reminders"] {
        for p in library.outbox_pending(service).unwrap() {
            assert_eq!(
                skykeep_mcp::sync::authorized(&library, dir.path(), &p),
                Ok(()),
                "{p:?}"
            );
        }
    }
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(mcp_refusals(&library), Vec::<String>::new(), "{result}");
    let requests = remote.received_requests().await.unwrap();
    assert!(handed_to_sender(&requests, &fresh));
    assert!(handed_to_sender(&requests, "note"));
}

/// Issue 74: the same, with the new etag arriving in the sync's own read from Apple, just
/// before the queued edit is checked and sent.
#[tokio::test]
async fn an_etag_changed_by_the_sync_read_still_sends_the_edit() {
    let (dir, mut server, library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"approved"}}),
    )
    .await;
    let remote = remote_with_other_in("list").await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(mcp_refusals(&library), Vec::<String>::new(), "{result}");
    assert!(handed_to_sender(
        &remote.received_requests().await.unwrap(),
        "other"
    ));
}

/// Issue 74: an item really changed on iCloud in the sync's own read is refused with a
/// reason and a next step, its grant is emptied, and it never reaches the sender.
#[tokio::test]
async fn an_item_moved_on_icloud_in_the_sync_read_is_refused_and_its_grant_emptied() {
    let (dir, mut server, library) = family().await;
    approved(
        &mut server,
        2,
        json!({"action":"edit","guid":"other","patch":{"title":"approved"}}),
    )
    .await;
    let id = pending_for(&library, "other").id;
    let remote = remote_with_other_in("list2").await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(
        refused_reason(&library, "other").as_deref(),
        Some("mcp_quarantine:item_changed_on_icloud_since_queued"),
        "{result}"
    );
    assert_eq!(grant(&library, id).as_deref(), Some(""));
    let refused = &result["refused_this_sync"][0];
    assert_eq!(refused["record"], "other", "{result}");
    let step = refused["instructions"].as_str().unwrap_or_default();
    assert!(step.contains("after this change was queued"), "{result}");
    assert!(step.contains("reminders_get"), "{result}");
    assert!(step.contains("nothing to redo"), "{result}");
    assert!(!modified(&remote.received_requests().await.unwrap())
        .iter()
        .any(|b| b.contains("approved")));
}

/// Issue 74: a body changed or an item deleted on iCloud before the sync is refused the
/// same way, each write alone.
#[tokio::test]
async fn a_body_changed_or_item_deleted_on_icloud_is_refused_with_a_reason() {
    let (dir, mut server, mut library) = family().await;
    let saved = write(
        &mut server,
        2,
        "notes_write",
        json!({"action":"save","guid":"note","body":"note\nbody edited","base_etag":"n1"}),
    )
    .await;
    assert_eq!(saved["isError"], false, "{saved}");
    approved(
        &mut server,
        3,
        json!({"action":"tick","guid":"child","done":true}),
    )
    .await;
    let note_id = library.outbox_pending("notes").unwrap()[0].id;
    let child_id = pending_for(&library, "child").id;
    library
        .set_note_text("note", "note\nchanged on a phone", "note", Some("changed"))
        .unwrap();
    library
        .forget_reminders(&["child".to_string()], &[])
        .unwrap();
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    for (id, reason) in library
        .outbox_refused()
        .unwrap()
        .iter()
        .map(|r| (r.change.id, &r.reason))
    {
        assert!([note_id, child_id].contains(&id), "{result}");
        assert_eq!(
            reason, "mcp_quarantine:item_changed_on_icloud_since_queued",
            "{result}"
        );
        assert_eq!(grant(&library, id).as_deref(), Some(""));
    }
    assert_eq!(library.outbox_refused().unwrap().len(), 2, "{result}");
    let requests = remote.received_requests().await.unwrap();
    assert!(!handed_to_sender(&requests, "note"));
    assert!(!handed_to_sender(&requests, "child"));
    // The terminal's quarantine list gives the same next step under each row.
    use std::os::unix::fs::PermissionsExt;
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_file() {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let rows = server.quarantined().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(
            row["next_step"]
                .as_str()
                .is_some_and(|t| t.contains("nothing to redo")),
            "{row}"
        );
    }
}

/// Review round 1: a change on iCloud to a field the queued write does not set (the title,
/// the due date) still sets a queued tick aside.
#[tokio::test]
async fn a_title_or_due_date_changed_on_icloud_refuses_a_queued_tick() {
    let (dir, mut server, mut library) = family().await;
    for (id, guid) in [(2, "other"), (3, "parent")] {
        approved(
            &mut server,
            id,
            json!({"action":"tick","guid":guid,"done":true}),
        )
        .await;
    }
    let mut other = library.reminder("other").unwrap().unwrap();
    other.title = "renamed on a phone".into();
    library.put_reminder_local(&other).unwrap();
    let mut parent = library.reminder("parent").unwrap().unwrap();
    parent.due_at = Some(1_800_000_000);
    library.put_reminder_local(&parent).unwrap();
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    for guid in ["other", "parent"] {
        assert_eq!(
            refused_reason(&library, guid).as_deref(),
            Some("mcp_quarantine:item_changed_on_icloud_since_queued"),
            "{guid}: {result}"
        );
    }
}

/// Review round 1: a formatting-only change to a note (its runs) after a save was queued sets
/// the save aside like a body change.
#[tokio::test]
async fn a_formatting_change_on_icloud_refuses_a_queued_save() {
    let (dir, mut server, mut library) = family().await;
    let saved = write(
        &mut server,
        2,
        "notes_write",
        json!({"action":"save","guid":"note","body":"note\nbody edited","base_etag":"n1"}),
    )
    .await;
    assert_eq!(saved["isError"], false, "{saved}");
    let p = library.outbox_pending("notes").unwrap().remove(0);
    assert_eq!(
        skykeep_mcp::sync::authorized(&library, dir.path(), &p),
        Ok(())
    );
    library
        .set_note_runs("note", &json!([{"length":4,"bold":true}]).to_string())
        .unwrap();
    let remote = remote().await;
    let result =
        skykeep_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
            .await
            .unwrap();
    assert_eq!(
        library.outbox_refused().unwrap()[0].reason,
        "mcp_quarantine:item_changed_on_icloud_since_queued",
        "{result}"
    );
    assert!(!handed_to_sender(
        &remote.received_requests().await.unwrap(),
        "note"
    ));
}
