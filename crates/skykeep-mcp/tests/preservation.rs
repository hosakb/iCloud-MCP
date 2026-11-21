//! What a supported MCP edit keeps, refuses and reports. Each test name is one row of the
//! tested capability matrix for note and reminder edits. Synthetic fixtures only.

use quince_core::notes::Note;
use quince_core::reminders::{
    EarlyAlert, Reminder, ReminderAttachment, ReminderDetailsPass, ReminderHashtag, ReminderList,
};
use quince_core::Library;
use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

fn setup() -> (tempfile::TempDir, Server, Library) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::open(dir.path()).unwrap();
    let mut library = Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic-account").unwrap();
    (dir, server, library)
}

async fn call(server: &mut Server, name: &str, args: Value) -> Value {
    server.handle(json!({"id":1,"method":"initialize"})).await;
    let response = server
        .handle(json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A title, two paragraphs, a table between them and a picture at the end.
const RICH: &str = "Trip\nPack the bag\n\u{FFFC}\nBook the train\n\u{FFFC}";

fn rich_runs() -> Value {
    json!([
        {"length":5,"style":0},
        {"length":13},
        {"length":1,"attachment":"TABLE-1"},
        {"length":1},
        {"length":15,"bold":true},
        {"length":1,"attachment":"IMG-1"}
    ])
}

fn put_note(library: &mut Library, body: &str, runs: Option<Value>, etag: &str) {
    library
        .put_notes(&[Note {
            guid: "note".into(),
            folder_guid: None,
            title: "Trip".into(),
            snippet: None,
            body: Some(body.into()),
            locked: false,
            pinned: false,
            created_at: None,
            modified_at: Some(1),
            editable: true,
            etag: Some(etag.into()),
            runs: runs.map(|r| r.to_string()),
        }])
        .unwrap();
}

fn rich_note(library: &mut Library) {
    put_note(library, RICH, Some(rich_runs()), "etag-1");
    library
        .set_state("notes.attachment.TABLE-1", r#"{"kind":"table"}"#)
        .unwrap();
    library
        .set_state(
            "notes.attachment.IMG-1",
            r#"{"kind":"image","hasPreview":true}"#,
        )
        .unwrap();
}

fn save(body: &str) -> Value {
    json!({"action":"save","guid":"note","base_etag":"etag-1","body":body})
}

#[tokio::test]
async fn note_paragraph_edit_keeps_adjacent_table_and_picture() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let edited = RICH.replace("Book the train", "Book the night train");
    let result = call(&mut server, "notes_write", save(&edited)).await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["kept_attachments"], 2, "{result}");
    assert_eq!(result["formatting"], "kept_from_apple", "{result}");
    let pending = library.outbox_pending("notes").unwrap();
    assert_eq!(pending.len(), 1);
    let payload: Value = serde_json::from_str(&pending[0].payload).unwrap();
    // Sent as a text edit over Apple's own document: no runs of ours replace Apple's.
    assert_eq!(payload["runs"], Value::Null);
    assert_eq!(payload["base"], RICH);
    assert_eq!(payload["base_runs"][2]["attachment"], "TABLE-1");
    assert_eq!(payload["base_runs"][5]["attachment"], "IMG-1");
    assert_eq!(
        library.note_body("note").unwrap().as_deref(),
        Some(&*edited)
    );
}

#[tokio::test]
async fn note_save_that_drops_a_table_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let edited = RICH.replacen('\u{FFFC}', "", 1);
    let result = call(&mut server, "notes_write", save(&edited)).await;
    assert_eq!(
        result["error"], "unsupported_attachment_removal",
        "{result}"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
    assert_eq!(library.note_body("note").unwrap().as_deref(), Some(RICH));
}

#[tokio::test]
async fn note_save_that_adds_an_attachment_character_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let edited = format!("{RICH}\n\u{FFFC}");
    let result = call(&mut server, "notes_write", save(&edited)).await;
    assert_eq!(
        result["error"], "unsupported_attachment_insertion",
        "{result}"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_save_that_moves_a_table_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    // The table line and the paragraph below it swapped: the same two attachment marks, but
    // the table's own mark is deleted and a new, empty one typed elsewhere.
    let moved = "Trip\nPack the bag\nBook the train\n\u{FFFC}\n\u{FFFC}";
    let result = call(&mut server, "notes_write", save(moved)).await;
    assert_eq!(result["error"], "unsupported_attachment_move", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
    assert_eq!(library.note_body("note").unwrap().as_deref(), Some(RICH));
}

#[tokio::test]
async fn note_saves_that_add_up_to_a_table_move_are_refused() {
    let (_dir, mut server, mut library) = setup();
    put_note(
        &mut library,
        "ab\u{FFFC}",
        Some(json!([{"length":2},{"length":1,"attachment":"TABLE-1"}])),
        "etag-1",
    );
    // Each save keeps the table's mark, but the one send, from the first text to the last,
    // would take it out and type a new one.
    let first = call(&mut server, "notes_write", save("ab\u{FFFC}ab")).await;
    assert_eq!(first["status"], "queued", "{first}");
    let second = call(&mut server, "notes_write", save("\u{FFFC}ab")).await;
    assert_eq!(second["error"], "unsupported_attachment_move", "{second}");
    let pending = library.outbox_pending("notes").unwrap();
    assert_eq!(pending.len(), 1);
    let payload: Value = serde_json::from_str(&pending[0].payload).unwrap();
    assert_eq!(payload["body"], "ab\u{FFFC}ab");
}

#[tokio::test]
async fn note_edits_on_both_sides_of_a_table_are_kept_apart() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let edited = RICH
        .replace("Pack the bag", "Pack the red bag")
        .replace("Book the train", "Book the late train");
    let result = call(&mut server, "notes_write", save(&edited)).await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["kept_attachments"], 2);
}

#[tokio::test]
async fn note_save_with_runs_naming_a_new_attachment_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let mut runs = rich_runs();
    runs[5]["attachment"] = json!("SOMETHING-ELSE");
    let mut args = save(RICH);
    args["runs"] = runs;
    let result = call(&mut server, "notes_write", args).await;
    assert_eq!(
        result["error"], "unsupported_attachment_insertion",
        "{result}"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_save_with_runs_that_do_not_describe_the_body_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let mut args = save(&RICH.replace("Pack", "Pack up"));
    // Formatting read for the old text, sent with the new.
    args["runs"] = rich_runs();
    let result = call(&mut server, "notes_write", args).await;
    assert_eq!(result["error"], "runs_do_not_match_body", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_save_from_a_markdown_export_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let runs: Vec<quince_icloud::ckdocument::Run> = serde_json::from_value(rich_runs()).unwrap();
    let markdown = quince_icloud::markdown::note_to_markdown(RICH, &runs);
    let result = call(&mut server, "notes_write", save(&markdown)).await;
    assert_eq!(result["error"], "lossy_markdown_overwrite", "{result}");
    // Editing one paragraph of the export is refused the same way.
    let edited = markdown.replace("Pack the bag", "Pack the big bag");
    let result = call(&mut server, "notes_write", save(&edited)).await;
    assert_eq!(result["error"], "lossy_markdown_overwrite", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
    assert_eq!(library.note_body("note").unwrap().as_deref(), Some(RICH));
}

#[tokio::test]
async fn note_save_typing_markdown_into_a_plain_note_is_text() {
    let (_dir, mut server, mut library) = setup();
    put_note(&mut library, "Plan\nToday", None, "etag-1");
    let result = call(&mut server, "notes_write", save("Plan\n- milk\n**bold**")).await;
    assert_eq!(result["status"], "queued", "{result}");
}

#[tokio::test]
async fn note_save_typing_a_heading_line_into_a_plain_note_is_text() {
    let (_dir, mut server, mut library) = setup();
    // The export makes the title a heading; the same line typed under the title is the
    // user's own text, not the export written back.
    put_note(&mut library, "Trip\nToday", None, "etag-1");
    let result = call(&mut server, "notes_write", save("Trip\n# Trip\nToday")).await;
    assert_eq!(result["status"], "queued", "{result}");
}

#[tokio::test]
async fn note_save_of_a_plain_note_s_markdown_export_is_refused() {
    let (_dir, mut server, mut library) = setup();
    put_note(&mut library, "Trip\nToday", None, "etag-1");
    let markdown = quince_icloud::markdown::note_to_markdown("Trip\nToday", &[]);
    let result = call(&mut server, "notes_write", save(&markdown)).await;
    assert_eq!(result["error"], "lossy_markdown_overwrite", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_save_over_a_very_long_note_finishes_without_comparing_every_line() {
    let (_dir, mut server, mut library) = setup();
    // An imported note far longer than one request, replaced by a short new text: comparing
    // every line with every line took billions of steps while the state was locked.
    let long: String = (0..400_000).map(|i| format!("l{i}\n")).collect();
    put_note(&mut library, &long, None, "etag-1");
    let short: String = (0..4_500).map(|i| format!("x{i}\n")).collect();
    let started = std::time::Instant::now();
    let result = call(&mut server, "notes_write", save(&short)).await;
    let took = started.elapsed();
    assert_eq!(result["status"], "queued", "{result}");
    assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
}

#[tokio::test]
async fn note_save_over_a_very_long_note_with_an_export_line_is_refused() {
    let (_dir, mut server, mut library) = setup();
    let long: String = (0..400_000).map(|i| format!("l{i}\n")).collect();
    put_note(&mut library, &long, None, "etag-1");
    // Too long to compare line by line: the export's heading for the title is enough to
    // refuse, as before the closer comparison.
    let short: String = std::iter::once("# l0\n".to_string())
        .chain((0..4_500).map(|i| format!("x{i}\n")))
        .collect();
    let started = std::time::Instant::now();
    let result = call(&mut server, "notes_write", save(&short)).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(result["error"], "lossy_markdown_overwrite", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_save_after_a_newer_sync_reports_conflict() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    // Read at etag-1; another device's edit arrives with etag-2 before the save.
    put_note(
        &mut library,
        &RICH.replace("Pack", "Repack"),
        Some(rich_runs()),
        "etag-2",
    );
    let result = call(&mut server, "notes_write", save(RICH)).await;
    assert_eq!(result["error"], "conflict", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn note_saves_queued_from_one_read_merge_into_one_send() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let first = RICH.replace("Pack the bag", "Pack the red bag");
    let second = first.replace("Book the train", "Book the late train");
    assert_eq!(
        call(&mut server, "notes_write", save(&first)).await["status"],
        "queued"
    );
    assert_eq!(
        call(&mut server, "notes_write", save(&second)).await["status"],
        "queued"
    );
    let pending = library.outbox_pending("notes").unwrap();
    assert_eq!(
        pending.len(),
        1,
        "one waiting save, sent from the first base"
    );
    let payload: Value = serde_json::from_str(&pending[0].payload).unwrap();
    assert_eq!(payload["base"], RICH);
    assert_eq!(payload["body"], second);
}

#[tokio::test]
async fn notes_get_reports_attachments_and_incomplete_reads() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    let result = call(&mut server, "notes_get", json!({"guid":"note"})).await;
    assert_eq!(result["complete_representation"], false, "{result}");
    assert_eq!(
        result["attachments"],
        json!([
            {"id":"TABLE-1","kind":"table","offset":18},
            {"id":"IMG-1","kind":"image","offset":35}
        ]),
        "{result}"
    );
    assert_eq!(
        result["incomplete_reads"],
        json!(["attachment_contents_not_included"])
    );
    let caps = &result["edit_capabilities"];
    assert_eq!(caps["edit_text"], true, "{result}");
    assert_eq!(caps["edit_formatting"], true);
    assert_eq!(caps["keep_attachments"], true);
    assert_eq!(caps["add_attachments"], false);
    assert_eq!(caps["remove_attachments"], false);
    assert_eq!(caps["edit_tables"], false);
    assert_eq!(caps["markdown_body"], false);
    assert_eq!(caps["conflict_check"], "base_etag");
}

#[tokio::test]
async fn notes_get_reports_missing_and_stale_formatting() {
    let (_dir, mut server, mut library) = setup();
    put_note(&mut library, "Plain\n\u{FFFC}", None, "etag-1");
    let result = call(&mut server, "notes_get", json!({"guid":"note"})).await;
    assert_eq!(
        result["incomplete_reads"],
        json!([
            "formatting_not_cached",
            "attachment_kinds_unknown",
            "attachment_contents_not_included"
        ]),
        "{result}"
    );
    assert_eq!(
        result["attachments"],
        json!([{"id":null,"kind":"unknown","offset":6}])
    );
    assert_eq!(result["edit_capabilities"]["edit_text"], true);
    assert_eq!(result["edit_capabilities"]["edit_formatting"], false);

    put_note(
        &mut library,
        "Longer text",
        Some(json!([{"length":2}])),
        "etag-2",
    );
    let result = call(&mut server, "notes_get", json!({"guid":"note"})).await;
    assert_eq!(
        result["incomplete_reads"],
        json!(["formatting_does_not_match_body"]),
        "{result}"
    );
    assert_eq!(result["edit_capabilities"]["edit_formatting"], false);
}

#[tokio::test]
async fn notes_get_reports_a_locked_note_as_unreadable_and_read_only() {
    let (_dir, mut server, mut library) = setup();
    library
        .put_notes(&[Note {
            guid: "note".into(),
            folder_guid: None,
            title: "Locked".into(),
            snippet: None,
            body: None,
            locked: true,
            pinned: false,
            created_at: None,
            modified_at: Some(1),
            editable: false,
            etag: Some("etag-1".into()),
            runs: None,
        }])
        .unwrap();
    let result = call(&mut server, "notes_get", json!({"guid":"note"})).await;
    assert_eq!(
        result["incomplete_reads"],
        json!(["locked", "body_not_cached", "formatting_not_cached"]),
        "{result}"
    );
    assert_eq!(result["edit_capabilities"]["edit_text"], false);
    assert_eq!(result["edit_capabilities"]["edit_formatting"], false);
}

// --- Reminders

fn rich_reminder(library: &mut Library) {
    library
        .put_reminder_lists(&[ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminders(&[Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "Water plants".into(),
            notes: Some("Balcony first".into()),
            due_at: Some(1_790_000_000),
            priority: 5,
            flagged: true,
            recur_frequency: Some(1),
            recur_interval: Some(2),
            etag: Some("etag-1".into()),
            raw: Some(r#"{"recordName":"r","fields":{}}"#.into()),
            timezone: Some("Europe/Prague".into()),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_details(&ReminderDetailsPass {
            whole: true,
            hashtags: vec![ReminderHashtag {
                guid: "Hashtag/h".into(),
                reminder_guid: "r".into(),
                name: "home".into(),
            }],
            attachments: vec![ReminderAttachment {
                guid: "Attachment/u".into(),
                reminder_guid: "r".into(),
                kind: "URL".into(),
                url: Some("https://example.invalid/plants".into()),
                uti: None,
            }],
            early_alerts: vec![(
                "r".into(),
                None,
                vec![EarlyAlert {
                    id: "a".into(),
                    count: 30,
                    unit: 0,
                }],
            )],
            smart_lists: vec![],
            gone: vec![],
        })
        .unwrap();
}

fn edit(patch: Value) -> Value {
    json!({"action":"edit","guid":"r","base_etag":"etag-1","patch":patch})
}

#[tokio::test]
async fn reminder_title_edit_keeps_recurrence_alerts_and_other_fields() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let before = library.reminder("r").unwrap().unwrap();
    let detail_before = library.reminder_detail("r").unwrap();
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"title":"Water herbs"})),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["changed_fields"], json!(["title"]));
    assert_eq!(result["conflict_check"], "base_etag");
    let after = library.reminder("r").unwrap().unwrap();
    assert_eq!(after.title, "Water herbs");
    assert_eq!(
        Reminder {
            title: before.title.clone(),
            ..after.clone()
        },
        before
    );
    assert_eq!(library.reminder_detail("r").unwrap(), detail_before);
    let pending = library.outbox_pending("reminders").unwrap();
    assert_eq!(pending.len(), 1);
    let payload: Value = serde_json::from_str(&pending[0].payload).unwrap();
    // Only the title travels; every other field is left as Apple holds it.
    assert_eq!(payload, json!({"title":"Water herbs","from_list":"list"}));
}

#[tokio::test]
async fn reminder_title_edit_reports_no_alarm_change() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"title":"Water herbs"})),
    )
    .await;
    assert_eq!(result["alarms"], "unchanged", "{result}");
}

#[tokio::test]
async fn reminder_date_edit_reports_that_the_date_alarm_follows() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"due_at":1_790_086_400})),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["alarms"], "date_alarm_moved_to_due_date", "{result}");
}

#[tokio::test]
async fn reminder_made_all_day_reports_that_the_date_alarm_goes() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"due_at":1_790_086_400,"all_day":true})),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["alarms"], "date_alarm_removed", "{result}");
}

#[tokio::test]
async fn reminder_edit_after_a_newer_sync_reports_conflict() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let mut newer = library.reminder("r").unwrap().unwrap();
    newer.etag = Some("etag-2".into());
    newer.raw = Some(r#"{"recordName":"r","fields":{}}"#.into());
    library.put_reminders(&[newer]).unwrap();
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"title":"Mine"})),
    )
    .await;
    assert_eq!(result["error"], "conflict", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
    assert_eq!(
        library.reminder("r").unwrap().unwrap().title,
        "Water plants"
    );
}

#[tokio::test]
async fn reminder_edit_without_base_etag_says_it_was_not_checked() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        json!({"action":"edit","guid":"r","patch":{"flagged":false}}),
    )
    .await;
    assert_eq!(result["status"], "queued", "{result}");
    assert_eq!(result["conflict_check"], "none");
}

#[tokio::test]
async fn reminder_edit_base_etag_survives_terminal_approval() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    library
        .set_state("mcp.require_write_approval", "true")
        .unwrap();
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"title":"Later"})),
    )
    .await;
    assert_eq!(result["status"], "approval_required", "{result}");
    let id = result["request_id"].as_str().unwrap().to_string();
    // A newer copy arrives while the request waits for the terminal.
    let mut newer = library.reminder("r").unwrap().unwrap();
    newer.etag = Some("etag-2".into());
    newer.raw = Some("{}".into());
    library.put_reminders(&[newer]).unwrap();
    assert!(server.approve(&id).is_err());
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn completing_a_repeating_reminder_by_edit_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"completed":true})),
    )
    .await;
    assert_eq!(
        result["error"], "repeating_reminder_complete_with_tick",
        "{result}"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn clearing_the_date_of_a_repeating_reminder_is_refused() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(&mut server, "reminders_write", edit(json!({"due_at":null}))).await;
    assert_eq!(
        result["error"], "repeating_reminder_needs_due_date",
        "{result}"
    );
    assert_eq!(library.outbox_count().unwrap(), 0);
    assert_eq!(
        library.reminder("r").unwrap().unwrap().due_at,
        Some(1_790_000_000)
    );
}

#[tokio::test]
async fn all_day_without_a_date_is_refused_not_ignored() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(
        &mut server,
        "reminders_write",
        edit(json!({"all_day":true})),
    )
    .await;
    assert_eq!(result["error"], "all_day_requires_due_at", "{result}");
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn reminders_get_reports_recurrence_and_what_it_leaves_out() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    let result = call(&mut server, "reminders_get", json!({"guid":"r"})).await;
    assert_eq!(result["complete_representation"], false);
    assert_eq!(result["item"]["recurFrequency"], 1, "{result}");
    assert_eq!(
        result["recurrence"],
        json!({"frequency":1,"interval":2,"days":[],"end":null})
    );
    assert_eq!(
        result["not_represented"],
        json!(["alarms", "attachment_contents", "assignment"])
    );
    assert_eq!(result["incomplete_reads"], json!([]));
    let caps = &result["edit_capabilities"];
    assert_eq!(caps["conflict_check"], "base_etag");
    assert_eq!(caps["complete_repeating_by_edit"], false);
    assert_eq!(caps["clear_date_of_repeating"], false);
    assert_eq!(
        caps["edit_fields"],
        json!([
            "title",
            "notes",
            "due_at",
            "all_day",
            "flagged",
            "completed",
            "priority",
            "list_guid",
            "parent_guid"
        ])
    );
}

#[tokio::test]
async fn reminders_get_reports_a_reminder_not_yet_read_from_apple() {
    let (_dir, mut server, mut library) = setup();
    rich_reminder(&mut library);
    library
        .put_reminder_local(&Reminder {
            guid: "local".into(),
            list_guid: "list".into(),
            title: "Made here".into(),
            ..Default::default()
        })
        .unwrap();
    let result = call(&mut server, "reminders_get", json!({"guid":"local"})).await;
    assert_eq!(
        result["incomplete_reads"],
        json!(["not_yet_read_from_apple"]),
        "{result}"
    );
}

#[tokio::test]
async fn reads_still_work_with_more_changes_waiting_than_one_send_takes() {
    let (_dir, mut server, mut library) = setup();
    rich_note(&mut library);
    rich_reminder(&mut library);
    // More waiting changes than a sync sends at once, none of them for these two items.
    for i in 0..130 {
        library
            .outbox_push("notes", &format!("other-{i}"), "pin", "{}")
            .unwrap();
    }
    library.outbox_push("reminders", "r", "done", "{}").unwrap();
    let note = call(&mut server, "notes_get", json!({"guid":"note"})).await;
    assert_eq!(note["item"]["guid"], "note", "{note}");
    assert_eq!(
        note["incomplete_reads"],
        json!(["attachment_contents_not_included"])
    );
    let reminder = call(&mut server, "reminders_get", json!({"guid":"r"})).await;
    assert_eq!(reminder["item"]["guid"], "r", "{reminder}");
    assert_eq!(
        reminder["incomplete_reads"],
        json!(["local_changes_not_sent"])
    );
}
