//! The `services:` line in permissions.yaml: which iCloud services the server may use. A
//! switched-off service's tools are not listed, a call to one is refused with the next step,
//! turning one on is reported as looser, a broken line never turns one on, and deletions
//! still ask. Synthetic data only, no network.
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

/// A private folder bound to a synthetic account with one reminder list and one reminder, in
/// `mode`, after an initialize that offers approval cards.
async fn folder(mode: Option<&str>) -> Folder {
    let dir = tempfile::Builder::new().prefix("state").tempdir().unwrap();
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

/// The tool result of a call, and whether it failed.
async fn result(f: &mut Folder, id: i64, name: &str, args: Value) -> (Value, bool) {
    let reply = f.server.handle(call(id, name, args)).await.unwrap();
    assert_ne!(reply["method"], "elicitation/create", "{reply}");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    (
        serde_json::from_str(text).unwrap(),
        reply["result"]["isError"] == true,
    )
}

async fn listed(f: &mut Folder) -> Vec<String> {
    let reply = f
        .server
        .handle(json!({"id":9,"method":"tools/list"}))
        .await
        .unwrap();
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn the_file_takes_a_services_list_on_one_line() {
    let parsed =
        permissions::parse_settings("version: 1\nservices: [calendar, notes]  # mine\n").unwrap();
    assert_eq!(
        parsed.services,
        Some(vec!["notes".to_string(), "calendar".to_string()]),
        "kept in the fixed order"
    );
    let none = permissions::parse_settings("services: []\n").unwrap();
    assert_eq!(none.services, Some(Vec::new()));
    let absent = permissions::parse_settings("version: 1\n").unwrap();
    assert_eq!(absent.services, None, "no line leaves every service on");
    for (text, says) in [
        (
            "services: [notes, mail]\n",
            "notes, reminders, contacts, calendar, photos and drive",
        ),
        ("services: [notes, notes]\n", "twice"),
        (
            "services: notes\n",
            "one line, such as services: [notes, calendar]",
        ),
        (
            "services:\n  - notes\n",
            "one line, such as services: [notes, calendar]",
        ),
        ("services: [notes]\nservices: [drive]\n", "listed twice"),
    ] {
        let error = permissions::parse_settings(text).unwrap_err();
        assert!(error.contains(says), "{text:?}: {error}");
    }
}

#[tokio::test]
async fn only_the_tools_of_services_that_are_on_are_listed() {
    let mut f = folder(Some("yolo")).await;
    let all = listed(&mut f).await;
    assert!(all.iter().any(|n| n == "photos_list"), "{all:?}");
    write_file(&f.path(), "version: 1\nservices: [reminders, calendar]\n");
    let names = listed(&mut f).await;
    for name in &names {
        let ok = name.starts_with("reminders_")
            || name.starts_with("calendar_")
            || ["sync", "sync_status", "auth_status"].contains(&name.as_str());
        assert!(ok, "{name} is listed: {names:?}");
    }
    for name in [
        "reminders_list",
        "calendar_events",
        "calendar_write",
        "sync",
    ] {
        assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
    }
    // sync refreshes only Notes and Reminders: its description names only what is on.
    let reply = f
        .server
        .handle(json!({"id":9,"method":"tools/list"}))
        .await
        .unwrap();
    let sync = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "sync")
        .unwrap();
    let text = sync["description"].as_str().unwrap();
    assert!(text.contains("Apple Reminders"), "{text}");
    assert!(!text.contains("Notes"), "{text}");
    // With Notes and Reminders both off, sync has nothing to do and is not listed.
    write_file(&f.path(), "version: 1\nservices: [photos]\n");
    let names = listed(&mut f).await;
    assert!(!names.iter().any(|n| n == "sync"), "{names:?}");
    assert!(names.iter().any(|n| n == "sync_status"), "{names:?}");
    assert!(names.iter().any(|n| n == "auth_status"), "{names:?}");
    // The server instructions name what is switched off.
    let mut server = Server::open(&f.path()).unwrap();
    let reply = server
        .handle(json!({"id":0,"method":"initialize"}))
        .await
        .unwrap();
    let instructions = reply["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains(
            "Switched off in this folder: Notes, Reminders, Contacts, Calendar and Drive"
        ),
        "{instructions}"
    );
}

#[tokio::test]
async fn a_call_to_a_switched_off_service_is_refused_with_the_next_step_at_the_next_call() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    let (_, failed) = result(&mut f, 1, "reminders_lists", json!({})).await;
    assert!(!failed);
    // Switched off after the client connected: the next call is refused, nothing is read.
    write_file(&dir, "version: 1\nservices: [notes]\n");
    let (value, failed) = result(&mut f, 2, "reminders_lists", json!({})).await;
    assert!(failed);
    assert_eq!(value["error"], "reminders_switched_off", "{value}");
    assert!(value.get("items").is_none(), "{value}");
    let message = value["message"].as_str().unwrap();
    assert!(message.contains("Reminders is switched off"), "{message}");
    let next = value["instructions"].as_str().unwrap();
    assert!(next.contains("services:"), "{next}");
    assert!(
        next.contains(&dir.join(permissions::FILE_NAME).display().to_string()),
        "{next}"
    );
    assert!(
        next.contains("cached"),
        "says what happens to the cached data: {next}"
    );
    // A change is refused before any card is shown, deletions included.
    let (value, failed) = result(
        &mut f,
        3,
        "reminders_write",
        json!({"action":"delete","guid":"R-1"}),
    )
    .await;
    assert!(failed);
    assert_eq!(value["error"], "reminders_switched_off", "{value}");
    // Its cached data stays until the owner clears it.
    assert!(f.library.reminder("R-1").unwrap().is_some());
    // sync_status still lists Reminders, marked as switched off, and names what is off.
    let (value, failed) = result(&mut f, 4, "sync_status", json!({})).await;
    assert!(!failed);
    let reminders = value["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "reminders")
        .unwrap();
    assert_eq!(reminders["switched_off"], true, "{value}");
    assert!(
        value["switched_off"]
            .as_array()
            .unwrap()
            .contains(&json!("reminders")),
        "{value}"
    );
    // Off for Notes and Reminders, sync itself is refused without reaching Apple.
    write_file(&dir, "version: 1\nservices: []\n");
    let (value, failed) = result(&mut f, 5, "sync", json!({})).await;
    assert!(failed);
    assert_eq!(value["error"], "sync_switched_off", "{value}");
    assert!(f.server.refresh().await.is_err());
}

#[tokio::test]
async fn turning_a_service_on_is_listed_as_looser_until_the_person_looks() {
    let mut f = folder(Some("writes_only")).await;
    let dir = f.path();
    write_file(&dir, "version: 1\nservices: [reminders]\n");
    f.server.handle(call(1, "reminders_lists", json!({}))).await;
    let shown = permissions::show(&dir, true).unwrap();
    assert!(
        shown.contains(
            "Services on: Reminders. Switched off: Notes, Contacts, Calendar, Photos and Drive."
        ),
        "{shown}"
    );
    assert!(
        shown.contains("stricter: services: Photos was on before, now switched off"),
        "{shown}"
    );
    write_file(&dir, "version: 1\nservices: [reminders, photos]\n");
    f.server.handle(call(2, "reminders_lists", json!({}))).await;
    let lines = permissions::status(&f.library, &dir, Some("writes_only"));
    let notice = &lines
        .iter()
        .find(|(t, _)| t.contains("became looser"))
        .expect("reported")
        .0;
    assert!(
        notice.contains("services: Photos was switched off before, now on"),
        "{notice}"
    );
    assert!(
        lines
            .iter()
            .any(|(t, _)| t.contains("Services on: Reminders and Photos.")),
        "{lines:?}"
    );
    let shown = permissions::show(&dir, true).unwrap();
    assert!(
        shown.contains("looser: services: Photos was switched off before, now on"),
        "{shown}"
    );
    // Removing the line turns every service on again: also looser.
    write_file(&dir, "version: 1\n");
    f.server.handle(call(3, "reminders_lists", json!({}))).await;
    let shown = permissions::show(&dir, true).unwrap();
    assert!(
        shown.contains("looser: services: Notes was switched off before, now on"),
        "{shown}"
    );
    assert!(shown.contains("Services on: all six"), "{shown}");
}

#[tokio::test]
async fn a_broken_line_never_turns_a_service_on() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    // Broken from the start, with no valid file loaded before: a services line that can't be
    // read switches every service off.
    write_file(&dir, "version: 1\nservices: [reminders, photoz]\n");
    let (value, failed) = result(&mut f, 1, "reminders_lists", json!({})).await;
    assert!(failed, "{value}");
    assert_eq!(value["error"], "reminders_switched_off", "{value}");
    // A valid line, then a broken edit that would add a service: the last valid line stays.
    write_file(&dir, "version: 1\nservices: [reminders]\n");
    let (_, failed) = result(&mut f, 2, "reminders_lists", json!({})).await;
    assert!(!failed);
    write_file(&dir, "version: 1\nservices: [reminders, photos\n");
    let (value, failed) = result(&mut f, 3, "photos_albums", json!({})).await;
    assert!(failed);
    assert_eq!(value["error"], "photos_switched_off", "{value}");
    let (_, failed) = result(&mut f, 4, "reminders_lists", json!({})).await;
    assert!(!failed);
    // A broken file elsewhere also keeps the last valid line.
    write_file(&dir, "version: 1\nbogus: 1\n");
    let (value, _) = result(&mut f, 5, "photos_albums", json!({})).await;
    assert_eq!(value["error"], "photos_switched_off", "{value}");
}

#[tokio::test]
async fn a_broken_file_without_a_services_line_and_no_history_leaves_every_service_on() {
    let mut f = folder(Some("yolo")).await;
    write_file(&f.path(), "version: 1\nbogus: 1\n");
    let (value, failed) = result(&mut f, 1, "reminders_lists", json!({})).await;
    assert!(!failed, "{value}");
}

#[tokio::test]
async fn an_unreadable_file_with_no_history_switches_every_service_off() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    write_file(&dir, "version: 1\nservices: [reminders]\n");
    std::fs::set_permissions(
        dir.join(permissions::FILE_NAME),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let (value, failed) = result(&mut f, 1, "reminders_lists", json!({})).await;
    assert!(failed, "{value}");
    assert_eq!(value["error"], "reminders_switched_off", "{value}");
}

#[tokio::test]
async fn deletions_still_ask_with_a_services_line_in_yolo() {
    let mut f = folder(Some("yolo")).await;
    write_file(
        &f.path(),
        "version: 1\nservices: [reminders]\nrules:\n  reminders_write.delete: allow\n",
    );
    let reply = f
        .server
        .handle(call(
            1,
            "reminders_write",
            json!({"action":"delete","guid":"R-1"}),
        ))
        .await
        .unwrap();
    assert_eq!(reply["method"], "elicitation/create", "{reply}");
}

#[test]
fn the_starter_file_explains_the_services_line_and_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let text = permissions::starter(dir.path());
    assert!(
        text.contains("# services: [notes, reminders, contacts, calendar, photos, drive]"),
        "{text}"
    );
    assert!(text.contains("not at Apple"), "{text}");
    let parsed = permissions::parse_settings(&text).unwrap();
    assert_eq!(parsed.services, None);
    // Uncommented, the example line switches nothing off.
    let on = text.replace("# services: [notes", "services: [notes");
    assert_eq!(
        permissions::parse_settings(&on)
            .unwrap()
            .services
            .map(|s| s.len()),
        Some(6)
    );
}

#[test]
fn rule_names_of_a_switched_off_service_stay_valid() {
    let parsed = permissions::parse_settings(
        "version: 1\nservices: [notes]\nrules:\n  photos_write.trash:\n    ask: client\n",
    )
    .unwrap();
    assert_eq!(parsed.rules.len(), 1);
}

#[tokio::test]
async fn a_missing_or_renamed_file_never_turns_a_service_on() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    write_file(&dir, "version: 1\nservices: [reminders]\n");
    let (_, failed) = result(&mut f, 1, "reminders_lists", json!({})).await;
    assert!(!failed);
    std::fs::rename(
        dir.join(permissions::FILE_NAME),
        dir.join("permissions.yaml.old"),
    )
    .unwrap();
    for id in 2..4 {
        let (value, failed) = result(&mut f, id, "photos_albums", json!({})).await;
        assert!(failed, "{value}");
        assert_eq!(value["error"], "photos_switched_off", "{value}");
    }
    let names = listed(&mut f).await;
    assert!(!names.iter().any(|n| n == "photos_list"), "{names:?}");
    let shown = permissions::show(&dir, true).unwrap();
    assert!(shown.contains("Services on: Reminders."), "{shown}");
    // Broken after it went missing: still not on.
    write_file(&dir, "version: 1\nbogus: 1\n");
    let (value, _) = result(&mut f, 4, "photos_albums", json!({})).await;
    assert_eq!(value["error"], "photos_switched_off", "{value}");
    // A valid file without the line is the way back to every service.
    write_file(&dir, "version: 1\n");
    let (_, failed) = result(&mut f, 5, "photos_albums", json!({})).await;
    assert!(!failed);
}

#[tokio::test]
async fn a_byte_order_mark_does_not_hide_a_broken_services_line() {
    let mut f = folder(Some("yolo")).await;
    write_file(&f.path(), "\u{feff}services: [reminders]\nbogus: 1\n");
    let (value, failed) = result(&mut f, 1, "photos_albums", json!({})).await;
    assert!(failed, "{value}");
    assert_eq!(value["error"], "photos_switched_off", "{value}");
    let (_, failed) = result(&mut f, 2, "reminders_lists", json!({})).await;
    assert!(!failed);
}

#[tokio::test]
async fn queued_changes_of_a_switched_off_service_are_set_aside_and_never_sent_later() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    let (value, failed) = result(
        &mut f,
        1,
        "reminders_write",
        json!({"action":"create","list_guid":"list","title":"Synthetic queued"}),
    )
    .await;
    assert!(!failed, "{value}");
    assert_eq!(f.library.outbox_pending("reminders").unwrap().len(), 1);
    write_file(&dir, "version: 1\nservices: [notes]\n");
    let (value, _) = result(&mut f, 2, "sync_status", json!({})).await;
    let reminders = value["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "reminders")
        .unwrap()
        .clone();
    assert_eq!(reminders["pending"], 0, "{value}");
    assert_eq!(reminders["refused"], 1, "{value}");
    assert_eq!(
        reminders["refused_recent"][0]["reason"], "mcp_quarantine:service_switched_off",
        "{value}"
    );
    // Switched on again, the change stays set aside: it needs a new request.
    write_file(&dir, "version: 1\n");
    result(&mut f, 3, "sync_status", json!({})).await;
    assert!(f.library.outbox_pending("reminders").unwrap().is_empty());
}

#[tokio::test]
async fn a_busy_folder_never_makes_services_look_switched_off() {
    let mut f = folder(Some("yolo")).await;
    let dir = f.path();
    let all = listed(&mut f).await;
    // Another process holds the folder: the list comes from the file itself.
    write_file(&dir, "version: 1\nservices: [reminders]\n");
    let held = skykeep_mcp::state::PrivateState::open(&dir).unwrap();
    let names = listed(&mut f).await;
    assert!(names.iter().any(|n| n == "reminders_list"), "{names:?}");
    assert!(!names.iter().any(|n| n == "photos_list"), "{names:?}");
    // No services line, folder busy: every tool is listed, nothing is called off.
    write_file(&dir, "version: 1\n");
    assert_eq!(listed(&mut f).await, all);
    let mut server = Server::open(&dir).unwrap();
    let reply = server
        .handle(json!({"id":0,"method":"initialize"}))
        .await
        .unwrap();
    let instructions = reply["result"]["instructions"].as_str().unwrap();
    assert!(!instructions.contains("Switched off"), "{instructions}");
    drop(held);
}
