//! A reminder created with a daily repeat, an alert at its time, an early alert and a zone:
//! checked, queued with the ids its records will have, and reported as queued only. Every
//! fixture is synthetic and every remote a loopback mock.
use icloud_mcp::Server;
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// 9:00 on 7 March 2026 in New York, the day before its clocks go forward.
const DUE: i64 = 1_772_892_000;

fn setup(account: bool) -> (tempfile::TempDir, Server, quince_core::Library) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    if account {
        // An early alert a phone set on another reminder, with the account id Apple writes.
        library
            .put_reminder_local(&quince_core::reminders::Reminder {
                guid: "Reminder/OTHER".into(),
                list_guid: "list".into(),
                title: "Synthetic other".into(),
                ..Default::default()
            })
            .unwrap();
        library
            .put_reminder_details(&quince_core::reminders::ReminderDetailsPass {
                early_alerts: vec![(
                    "Reminder/OTHER".into(),
                    Some("SYNTHETIC-ALERT-ACCOUNT".into()),
                    vec![quince_core::reminders::EarlyAlert {
                        id: "E".into(),
                        count: 5,
                        unit: 0,
                    }],
                )],
                ..Default::default()
            })
            .unwrap();
    }
    (dir, server, library)
}

async fn call(server: &mut Server, args: Value) -> (bool, Value) {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let response = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":"reminders_write","arguments":args}}))
        .await
        .unwrap();
    (
        response["result"]["isError"] == true,
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap(),
    )
}

fn full() -> Value {
    json!({
        "action":"create","list_guid":"list","title":"Synthetic daily","due_at":DUE,
        "repeat":{"frequency":"daily","until":"2026-03-20"},
        "due_time_alert":true,
        "early_alert":{"amount":15,"unit":"minutes"},
        "timezone":"America/New_York"
    })
}

#[tokio::test]
async fn a_create_with_a_repeat_and_alerts_is_queued_and_reported_as_queued_only() {
    let (_dir, mut server, library) = setup(true);
    let (failed, result) = call(&mut server, full()).await;
    assert!(!failed, "{result}");
    assert_eq!(result["status"], "queued");
    assert_eq!(result["queued"], true);
    assert_eq!(result["cloudkit_accepted"], false);
    assert_eq!(result["remote_verified"], false);
    // The end of 20 March in New York (EDT, UTC-4): 03:59:59 UTC on 21 March.
    assert_eq!(
        result["options"],
        json!({
            "timezone":"America/New_York",
            "repeat":{"frequency":"daily","ends_at":1_774_065_599},
            "due_time_alert":true,
            "early_alert":{"amount":15,"unit":"minutes"}
        })
    );
    // The reply names no record ids and no account id.
    let text = result.to_string();
    assert!(!text.contains("SYNTHETIC-ALERT-ACCOUNT"), "{text}");
    assert!(
        !text.contains("rule_id") && !text.contains("alarm_id"),
        "{text}"
    );

    let queued = library.outbox_pending("reminders").unwrap();
    assert_eq!(queued.len(), 1);
    let payload: Value = serde_json::from_str(&queued[0].payload).unwrap();
    assert_eq!(payload["timezone"], "America/New_York");
    assert_eq!(payload["repeat"]["frequency"], 0);
    assert_eq!(payload["repeat"]["end_at"], 1_774_065_599);
    assert_eq!(payload["early_alert"]["account"], "SYNTHETIC-ALERT-ACCOUNT");
    assert_eq!(payload["early_alert"]["unit"], 0);
    for id in [
        &payload["repeat"]["rule_id"],
        &payload["alarm"]["alarm_id"],
        &payload["alarm"]["trigger_id"],
        &payload["early_alert"]["id"],
    ] {
        assert_eq!(id.as_str().map(str::len), Some(36), "{payload}");
    }
    let shown = library
        .reminder(result["guid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(shown.recur_frequency, Some(0));
    assert_eq!(shown.timezone.as_deref(), Some("America/New_York"));
}

#[tokio::test]
async fn combinations_that_cannot_work_are_refused_and_queue_nothing() {
    let base = || json!({"action":"create","list_guid":"list","title":"Synthetic"});
    let with = |extra: Value| {
        let mut v = base();
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let cases = [
        (
            json!({"due_time_alert":true}),
            "due_time_alert_requires_due_at",
        ),
        (
            json!({"due_at":DUE,"all_day":true,"due_time_alert":true}),
            "all_day_cannot_have_timed_alert",
        ),
        (
            json!({"due_at":DUE,"all_day":true,"early_alert":{"amount":1,"unit":"days"}}),
            "all_day_cannot_have_timed_alert",
        ),
        (
            json!({"early_alert":{"amount":5,"unit":"minutes"}}),
            "early_alert_requires_due_at",
        ),
        (
            json!({"due_at":DUE,"early_alert":{"amount":0,"unit":"minutes"}}),
            "invalid_early_alert",
        ),
        (
            json!({"due_at":DUE,"early_alert":{"amount":10_001,"unit":"minutes"}}),
            "invalid_early_alert",
        ),
        (
            json!({"due_at":DUE,"timezone":"Mars/Olympus"}),
            "unknown_timezone",
        ),
        (
            json!({"due_at":DUE,"timezone":"+05:00"}),
            "unknown_timezone",
        ),
        (
            json!({"due_at":DUE,"timezone":"x".repeat(65)}),
            "unknown_timezone",
        ),
        (
            json!({"due_at":DUE,"all_day":true,"timezone":"Europe/Berlin"}),
            "timezone_requires_timed_due_at",
        ),
        (
            json!({"timezone":"Europe/Berlin"}),
            "timezone_requires_timed_due_at",
        ),
        (
            json!({"repeat":{"frequency":"daily"}}),
            "repeat_requires_due_at",
        ),
        (
            json!({"due_at":DUE,"timezone":"America/New_York","repeat":{"frequency":"daily","until":"2026-03-06"}}),
            "repeat_end_before_due",
        ),
        (
            json!({"due_at":DUE,"repeat":{"frequency":"daily","until":"2026-02-30"}}),
            "invalid_repeat_end",
        ),
        (
            json!({"due_at":DUE,"repeat":{"frequency":"daily","until":"next week"}}),
            "invalid_repeat_end",
        ),
        (
            json!({"due_at":DUE,"repeat":{"frequency":"weekly"}}),
            "invalid_arguments",
        ),
        (
            json!({"due_at":DUE,"repeat":{"frequency":"daily","interval":2}}),
            "invalid_arguments",
        ),
        (
            json!({"due_at":DUE,"early_alert":{"amount":5,"unit":"seconds"}}),
            "invalid_arguments",
        ),
        (
            json!({"due_at":DUE,"due_time_alert":"yes"}),
            "invalid_arguments",
        ),
    ];
    for (extra, code) in cases {
        let (_dir, mut server, library) = setup(true);
        let (failed, result) = call(&mut server, with(extra.clone())).await;
        assert!(failed, "{extra}: {result}");
        assert_eq!(result["error"], code, "{extra}");
        assert_eq!(library.outbox_count().unwrap(), 0, "{extra}");
    }
    // No early alert anywhere on the account: no account id to write inside one.
    let (_dir, mut server, library) = setup(false);
    let (failed, result) = call(
        &mut server,
        with(json!({"due_at":DUE,"early_alert":{"amount":5,"unit":"minutes"}})),
    )
    .await;
    assert!(failed);
    // The code says what to do: set one early alert on an Apple device, then sync.
    assert_eq!(
        result["error"],
        "early_alert_account_unavailable_set_one_on_an_apple_device_then_sync"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
    // The options belong to a create; an edit carrying one is not understood.
    let (_dir, mut server, _library) = setup(true);
    let (failed, result) = call(
        &mut server,
        json!({"action":"edit","guid":"Reminder/X","patch":{"title":"t"},"due_time_alert":true}),
    )
    .await;
    assert!(failed);
    assert_eq!(result["error"], "invalid_arguments");
}

/// An all-day reminder can repeat daily; its last day is read in UTC, where an all-day date is
/// that day's midnight. A due-time alert of `false` asks for nothing.
#[tokio::test]
async fn an_all_day_reminder_repeats_until_its_last_day() {
    let (_dir, mut server, library) = setup(false);
    // Midnight UTC on 7 March 2026.
    let day = 1_772_841_600;
    let (failed, result) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic","due_at":day,"all_day":true,
               "due_time_alert":false,"repeat":{"frequency":"daily","until":"2026-03-07"}}),
    )
    .await;
    assert!(!failed, "{result}");
    assert_eq!(result["options"]["repeat"]["ends_at"], day + 86_399);
    assert_eq!(result["options"]["due_time_alert"], false);
    let payload: Value =
        serde_json::from_str(&library.outbox_pending("reminders").unwrap()[0].payload).unwrap();
    assert!(payload.get("alarm").is_none() && payload.get("timezone").is_none());
}

/// With no zone given, a timed create with options is kept in this computer's zone as it was
/// when queued: the repeat's end and every later send use that one zone. A plain create
/// queues no zone, as before.
#[tokio::test]
async fn the_default_zone_is_chosen_once_and_queued() {
    let (_dir, mut server, library) = setup(false);
    let (failed, result) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic","due_at":DUE,
               "repeat":{"frequency":"daily","until":"2026-03-20"}}),
    )
    .await;
    assert!(!failed, "{result}");
    let zone = result["options"]["timezone"]
        .as_str()
        .expect("a zone")
        .to_string();
    assert_eq!(
        quince_sync::reminders::iana_zone(&zone).as_deref(),
        Some(zone.as_str())
    );
    let payload: Value =
        serde_json::from_str(&library.outbox_pending("reminders").unwrap()[0].payload).unwrap();
    assert_eq!(payload["timezone"], zone.as_str());
    assert_eq!(
        payload["repeat"]["end_at"],
        quince_sync::reminders::end_of_day("2026-03-20", &zone).unwrap()
    );
    let (_, plain) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic plain","due_at":DUE}),
    )
    .await;
    let queued = library.outbox_pending("reminders").unwrap();
    let plain_payload: Value = serde_json::from_str(
        &queued
            .iter()
            .find(|p| p.record == plain["guid"].as_str().unwrap())
            .unwrap()
            .payload,
    )
    .unwrap();
    assert!(plain_payload.get("timezone").is_none(), "{plain_payload}");
}

/// Under the approval policy the options wait with the rest of the create on the card, and
/// the human's approval queues exactly them.
#[tokio::test]
async fn options_wait_for_approval_with_the_create() {
    let (_dir, mut server, mut library) = setup(true);
    library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
    let form = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":"reminders_write","arguments":full()}}))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(library.outbox_count().unwrap(), 0);
    // An underscore inside a word stays as it is on the card.
    let message = form["params"]["message"].as_str().unwrap();
    assert!(
        message.contains("2026-03-20") && message.contains("America/New_York"),
        "{message}"
    );
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
        .await
        .unwrap();
    let approved: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(approved["status"], "queued", "{approved}");
    assert_eq!(approved["remote_verified"], false);
    assert_eq!(approved["options"]["early_alert"]["amount"], 15);
    let payload: Value =
        serde_json::from_str(&library.outbox_pending("reminders").unwrap()[0].payload).unwrap();
    assert_eq!(payload["repeat"]["frequency"], 0);
}

#[tokio::test]
async fn the_schema_describes_the_new_create_fields() {
    let (_dir, mut server, _library) = setup(false);
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let list = server
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "reminders_write")
        .unwrap()
        .clone();
    let props = &tool["inputSchema"]["properties"];
    assert_eq!(
        props["repeat"]["properties"]["frequency"]["enum"],
        json!(["daily"])
    );
    assert_eq!(props["repeat"]["additionalProperties"], false);
    // An all-day reminder's last day is read in UTC, a timed one's in its zone.
    let until = props["repeat"]["properties"]["until"]["description"]
        .as_str()
        .unwrap();
    assert!(
        until.contains("UTC") && until.contains("all-day"),
        "{until}"
    );
    assert_eq!(props["due_time_alert"]["type"], "boolean");
    assert_eq!(
        props["early_alert"]["properties"]["amount"]["maximum"],
        10_000
    );
    assert_eq!(props["early_alert"]["additionalProperties"], false);
    assert_eq!(props["timezone"]["maxLength"], 64);
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
}

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

/// Sync names the writes CloudKit took, apart from read-back, which stays unverified. A
/// create whose repeat could not be written (this pretend Apple never has the reminder to
/// point at a rule) is not called taken: it waits, queued, to be sent again.
#[tokio::test]
async fn sync_names_only_writes_cloudkit_took_whole() {
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
    let (dir, mut server, mut library) = setup(false);
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    server.set_policy("yolo").unwrap();
    let (_, plain) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic plain"}),
    )
    .await;
    // Its date is written after the reminder, and a failure there keeps the create queued:
    // leaving the queue, it is accepted like any other create.
    let (_, dated) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic dated","due_at":DUE}),
    )
    .await;
    let (failed, repeating) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic daily","due_at":DUE,"repeat":{"frequency":"daily"}}),
    )
    .await;
    assert!(!failed, "{repeating}");
    let result = icloud_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(result["remote_verified"], false);
    assert!(result.get("cloudkit_accepted_this_sync").is_none());
    let left = result["left_queue_this_sync"].as_array().unwrap();
    assert_eq!(left.len(), 2, "{result}");
    assert_eq!(left[0]["record"], plain["guid"]);
    assert_eq!(left[0]["kind"], "create");
    assert_eq!(left[0]["outcome"], "cloudkit_accepted");
    assert_eq!(left[1]["record"], dated["guid"]);
    assert_eq!(left[1]["outcome"], "cloudkit_accepted");
    let waiting = library.outbox_pending("reminders").unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].record, repeating["guid"].as_str().unwrap());
    assert_eq!(waiting[0].tries, 1);
    assert!(!result.to_string().contains("SYNTHETIC-TOKEN"));
}

/// A pretend CloudKit that keeps the records written to it, so a create with options can be
/// sent whole.
#[derive(Clone, Default)]
struct Records(std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>);

impl Respond for Records {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut records = self.0.lock().unwrap();
        let answer = |r: Value| ResponseTemplate::new(200).set_body_json(json!({"records":[r]}));
        if request.url.path().ends_with("/records/lookup") {
            let name = body["records"][0]["recordName"].as_str().unwrap();
            return answer(
                records
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| json!({"recordName":name,"serverErrorCode":"NOT_FOUND"})),
            );
        }
        let op = &body["operations"][0];
        let name = op["record"]["recordName"].as_str().unwrap().to_string();
        let tag = format!("t{}", records.len() + 1);
        match op["operationType"].as_str().unwrap() {
            "create" => {
                let mut r = op["record"].clone();
                r["recordChangeTag"] = json!(tag);
                records.insert(name, r.clone());
                answer(r)
            }
            "update" => match records.get_mut(&name) {
                None => {
                    answer(json!({"recordName":name,"serverErrorCode":"NOT_FOUND","reason":"gone"}))
                }
                Some(r) => {
                    for (k, v) in op["record"]["fields"].as_object().unwrap() {
                        r["fields"][k] = v.clone();
                    }
                    r["recordChangeTag"] = json!(format!("{tag}u"));
                    answer(r.clone())
                }
            },
            _ => {
                records.remove(&name);
                answer(json!({"recordName":name}))
            }
        }
    }
}

/// Apple holds what is written: a create with a repeat and an alert is sent whole, leaves the
/// queue and is named accepted, the reminder pointing at the rule and alarm it was queued with.
#[tokio::test]
async fn sync_names_an_options_create_cloudkit_took_whole_as_accepted() {
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
    let records = Records::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/records/(lookup|modify)$"))
        .respond_with(records.clone())
        .mount(&remote)
        .await;
    let (dir, mut server, mut library) = setup(false);
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    server.set_policy("yolo").unwrap();
    let (failed, made) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic daily","due_at":DUE,
               "timezone":"America/New_York","repeat":{"frequency":"daily"},"due_time_alert":true}),
    )
    .await;
    assert!(!failed, "{made}");
    let payload: Value =
        serde_json::from_str(&library.outbox_pending("reminders").unwrap()[0].payload).unwrap();
    let result = icloud_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    let left = result["left_queue_this_sync"].as_array().unwrap();
    assert_eq!(left.len(), 1, "{result}");
    assert_eq!(left[0]["record"], made["guid"]);
    assert_eq!(left[0]["outcome"], "cloudkit_accepted");
    assert_eq!(result["remote_verified"], false);
    assert!(library.outbox_pending("reminders").unwrap().is_empty());
    // The reply says what accepted leaves out: options dropped on purpose, the list order.
    let note = result["left_queue_note"].as_str().unwrap();
    assert!(
        note.contains("left out on purpose") && note.contains("list-order"),
        "{note}"
    );
    let held = records.0.lock().unwrap();
    let reminder = &held[made["guid"].as_str().unwrap()];
    assert_eq!(
        reminder["fields"]["RecurrenceRuleIDs"]["value"],
        json!([payload["repeat"]["rule_id"]])
    );
    assert_eq!(
        reminder["fields"]["AlarmIDs"]["value"],
        json!([payload["alarm"]["alarm_id"]])
    );
}

fn steps_key(guid: &str) -> String {
    format!("reminders.create_steps.{guid}")
}

/// The refused row whose record is `guid`, and why.
fn refused_for(library: &quince_core::Library, guid: &str) -> (i64, String) {
    let r = library
        .outbox_refused()
        .unwrap()
        .into_iter()
        .find(|r| r.change.record == guid)
        .expect("refused");
    (r.change.id, r.reason)
}

/// A quarantined create keeps the note of its steps, as a refused one does, so one put back
/// takes only the steps still to do. The human's discard in the terminal takes it away.
async fn kept_until_discarded(
    server: &mut Server,
    library: &quince_core::Library,
    guid: &str,
    reason: &str,
) {
    let (id, why) = refused_for(library, guid);
    assert_eq!(why, reason);
    assert_eq!(
        library.state(&steps_key(guid)).unwrap().as_deref(),
        Some("2")
    );
    assert_eq!(server.retire_quarantined(&[id]).unwrap(), 1);
    assert_eq!(library.state(&steps_key(guid)).unwrap(), None);
}

#[tokio::test]
async fn a_create_quarantined_by_sync_keeps_its_step_note_until_discarded() {
    let remote = MockServer::start().await;
    let (dir, mut server, mut library) = setup(false);
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    server.set_policy("yolo").unwrap();
    let guid = "Reminder/SYNTHETIC-UNAPPROVED";
    library
        .outbox_append("reminders", guid, "create", "{}")
        .unwrap();
    library.set_state(&steps_key(guid), "2").unwrap();
    // No routes: the pass fails after the local gate has quarantined the write. The session
    // file is kept apart, so the private folder holds only what the server writes there.
    let session = tempfile::tempdir().unwrap();
    assert!(
        icloud_mcp::sync::with_client(dir.path(), saved_client(session.path(), &remote.uri()))
            .await
            .is_err()
    );
    kept_until_discarded(
        &mut server,
        &library,
        guid,
        "mcp_quarantine:legacy_write_quarantined",
    )
    .await;
}

#[tokio::test]
async fn a_create_quarantined_by_a_policy_change_keeps_its_step_note_until_discarded() {
    let (_dir, mut server, mut library) = setup(false);
    let (failed, made) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic","due_at":DUE,"repeat":{"frequency":"daily"}}),
    )
    .await;
    assert!(!failed, "{made}");
    let guid = made["guid"].as_str().unwrap();
    library.set_state(&steps_key(guid), "2").unwrap();
    server.set_policy("yolo").unwrap();
    kept_until_discarded(
        &mut server,
        &library,
        guid,
        "mcp_policy_migration_quarantine",
    )
    .await;
}

#[tokio::test]
async fn a_create_superseded_by_a_deletion_keeps_its_step_note_until_discarded() {
    let (_dir, mut server, mut library) = setup(false);
    server.set_policy("yolo").unwrap();
    // A client that shows the card: the deletion asks there, also under yolo.
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
    let (failed, made) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic","due_at":DUE,"repeat":{"frequency":"daily"}}),
    )
    .await;
    assert!(!failed, "{made}");
    let guid = made["guid"].as_str().unwrap();
    library.set_state(&steps_key(guid), "2").unwrap();
    let form = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":"reminders_write","arguments":{"action":"delete","guid":guid}}}))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let gone = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
        .await
        .unwrap();
    assert_eq!(gone["result"]["isError"], false, "{gone}");
    kept_until_discarded(
        &mut server,
        &library,
        guid,
        "superseded_by_approved_deletion",
    )
    .await;
}

/// A pretend Apple that makes a new reminder and answers its date or flag write with
/// `update`, so a dated create is made but its second write is not taken.
async fn apple_taking_only_the_create(update: ResponseTemplate) -> MockServer {
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
        .and(wiremock::matchers::body_string_contains(
            r#""operationType":"create""#,
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"records":[{"recordName":"synthetic"}]})),
        )
        .with_priority(1)
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/records/modify$"))
        .respond_with(update)
        .mount(&remote)
        .await;
    remote
}

/// `sync_status` as a client calls it. The synthetic session file `saved_client` wrote is made
/// private first, as the program's own umask keeps every file it writes.
async fn sync_status(server: &mut Server, dir: &std::path::Path) -> Value {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let response = server
        .handle(
            json!({"id":3,"method":"tools/call","params":{"name":"sync_status","arguments":{}}}),
        )
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A dated plain create whose date Apple was busy for stays queued, is not reported as having
/// left the queue, and `sync` and `sync_status` say why it waits with a fixed code, never
/// Apple's words.
#[tokio::test]
async fn a_dated_create_apple_was_busy_for_waits_and_says_why() {
    let remote = apple_taking_only_the_create(
        ResponseTemplate::new(503).set_body_string("SYNTHETIC-APPLE-WORDS"),
    )
    .await;
    let (dir, mut server, mut library) = setup(false);
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    server.set_policy("yolo").unwrap();
    let (failed, dated) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic dated","due_at":DUE,"flagged":true}),
    )
    .await;
    assert!(!failed, "{dated}");
    let result = icloud_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    assert_eq!(result["left_queue_this_sync"], json!([]), "{result}");
    assert_eq!(result["queued"], 1);
    let waiting = result["pending_recent"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{result}");
    assert_eq!(waiting[0]["service"], "reminders");
    assert_eq!(waiting[0]["kind"], "create");
    assert_eq!(waiting[0]["record"], dated["guid"]);
    assert_eq!(waiting[0]["reason"], "waiting_apple_busy");
    assert!(waiting[0].get("try").is_none(), "{result}");
    let step = waiting[0]["next_step"].as_str().unwrap();
    assert!(step.contains("Nothing to do"), "{step}");
    assert!(!result.to_string().contains("SYNTHETIC-APPLE-WORDS"));

    let status = sync_status(&mut server, dir.path()).await;
    let reminders = &status["services"][1];
    assert_eq!(reminders["service"], "reminders", "{status}");
    assert_eq!(reminders["pending"], 1);
    assert_eq!(reminders["pending_recent"], json!(waiting));
    assert_eq!(status["services"][0]["pending_recent"], json!([]));
    assert!(!status.to_string().contains("SYNTHETIC-APPLE-WORDS"));
}

/// An answer that can't be read is tried again a few times: the waiting change says which
/// try it is and how many there are.
#[tokio::test]
async fn a_create_whose_answer_was_not_understood_says_which_try() {
    let remote =
        apple_taking_only_the_create(ResponseTemplate::new(200).set_body_string("not json")).await;
    let (dir, mut server, mut library) = setup(false);
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    server.set_policy("yolo").unwrap();
    let (failed, dated) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic dated","due_at":DUE}),
    )
    .await;
    assert!(!failed, "{dated}");
    // A second change waits behind it for the same reason.
    let (failed, second) = call(
        &mut server,
        json!({"action":"create","list_guid":"list","title":"Synthetic second"}),
    )
    .await;
    assert!(!failed, "{second}");
    let status = sync_status(&mut server, dir.path()).await;
    let before = status["services"][1]["pending_recent"].as_array().unwrap();
    assert_eq!(before.len(), 2, "{status}");
    assert_eq!(before[0]["reason"], "waiting_to_send");
    assert!(before[0]["next_step"].as_str().unwrap().contains("sync"));
    for round in 1..=2 {
        let result =
            icloud_mcp::sync::with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
                .await
                .unwrap();
        let waiting = result["pending_recent"].as_array().unwrap();
        assert_eq!(waiting.len(), 2, "{result}");
        assert_eq!(waiting[0]["record"], dated["guid"]);
        assert_eq!(waiting[0]["reason"], "waiting_answer_not_understood");
        assert_eq!(waiting[0]["try"], round);
        assert_eq!(waiting[0]["tries_allowed"], 5);
        let step = waiting[0]["next_step"].as_str().unwrap();
        assert!(step.contains("run sync again"), "{step}");
        // Never sent: it waits behind the first, and says so rather than Apple's answer.
        assert_eq!(waiting[1]["record"], second["guid"]);
        assert_eq!(
            waiting[1]["reason"],
            format!("waiting_behind_change_{}", waiting[0]["id"])
        );
        assert!(waiting[1].get("try").is_none(), "{result}");
        let step = waiting[1]["next_step"].as_str().unwrap();
        assert!(
            step.contains(&format!("change {}", waiting[0]["id"])),
            "{step}"
        );
    }
}

/// The sync and sync_status descriptions say that waiting changes come with a reason.
#[tokio::test]
async fn the_sync_tools_describe_pending_recent() {
    let (_dir, mut server, _library) = setup(false);
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let tools = server
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    for name in ["sync", "sync_status"] {
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap();
        let text = tool["description"].as_str().unwrap();
        assert!(text.contains("pending_recent"), "{name}: {text}");
    }
}
