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
    // A prior full sync, so the synthetic pass is incremental and keeps these items.
    for service in ["notes", "reminders"] {
        library.mark_synced(service, Some("prior")).unwrap();
        library.mark_full_sync(service).unwrap();
    }
    server.set_policy("yolo").unwrap();
    server.handle(json!({"id":1,"method":"initialize"})).await;
    (dir, server, library)
}

async fn write(server: &mut skykeep_mcp::Server, id: i64, tool: &str, args: Value) -> Value {
    let reply = server
        .handle(json!({"id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}))
        .await
        .unwrap();
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
        Some("mcp_quarantine:write_authorization_stale")
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
