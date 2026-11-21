//! What a note or reminder edit keeps, refuses and reports, checked against the local copy
//! before anything is queued. The send checks again against what Apple holds then.

use quince_core::notes::Note;
use quince_core::reminders::Reminder;
use quince_core::Library;
use quince_icloud::ckdocument::Run;
use serde_json::{json, Map, Value};

/// The character a table, picture or other attachment stands on in a note's text.
const ATTACHMENT: char = '\u{FFFC}';

/// Reminder fields an edit may change. Everything else is left as Apple holds it.
pub(crate) const REMINDER_FIELDS: [&str; 9] = [
    "title",
    "notes",
    "due_at",
    "all_day",
    "flagged",
    "completed",
    "priority",
    "list_guid",
    "parent_guid",
];

fn utf16_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn describes(body: &str, runs: &[Run]) -> bool {
    runs.iter().map(|r| r.length).sum::<u64>() == utf16_len(body)
}

fn attachment_ids(runs: &[Run]) -> impl Iterator<Item = &str> {
    runs.iter().filter_map(|r| r.attachment.as_deref())
}

/// Where each attachment character sits (UTF-16 offset), with its id when the runs say.
fn attachments(body: &str, runs: Option<&[Run]>) -> Vec<(u64, Option<String>)> {
    let mut ids = std::collections::HashMap::new();
    if let Some(runs) = runs.filter(|r| describes(body, r)) {
        let mut at = 0;
        for r in runs {
            if let Some(id) = &r.attachment {
                ids.insert(at, id.clone());
            }
            at += r.length;
        }
    }
    let mut out = Vec::new();
    let mut at = 0;
    for c in body.chars() {
        if c == ATTACHMENT {
            out.push((at, ids.get(&at).cloned()));
        }
        at += c.len_utf16() as u64;
    }
    out
}

/// The fields `notes_get` adds: each attachment, what the cached copy lacks, and which edits
/// keep the rest of the note.
pub(crate) fn note_report(
    library: &Library,
    note: &Note,
    runs: Option<&str>,
) -> Result<Value, &'static str> {
    let body = note.body.as_deref();
    let parsed: Option<Result<Vec<Run>, _>> = runs.map(serde_json::from_str);
    let runs = parsed.as_ref().and_then(|r| r.as_ref().ok());
    let mut incomplete = Vec::new();
    if note.locked {
        incomplete.push("locked");
    }
    if body.is_none() {
        incomplete.push("body_not_cached");
    }
    let formatting = match (&parsed, body) {
        (None, _) => Some("formatting_not_cached"),
        (Some(Err(_)), _) => Some("formatting_unreadable"),
        (Some(Ok(runs)), Some(body)) if !describes(body, runs) => {
            Some("formatting_does_not_match_body")
        }
        _ => None,
    };
    incomplete.extend(formatting);
    let mut list = Vec::new();
    for (offset, id) in attachments(body.unwrap_or(""), runs.map(Vec::as_slice)) {
        let kind = match &id {
            Some(id) => library
                .state(&format!("notes.attachment.{id}"))
                .map_err(|_| "library_unavailable")?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|v| v["kind"].as_str().map(str::to_string)),
            None => None,
        };
        list.push(json!({"id":id,"kind":kind.as_deref().unwrap_or("unknown"),"offset":offset}));
    }
    if list.iter().any(|a| a["kind"] == "unknown") {
        incomplete.push("attachment_kinds_unknown");
    }
    if !list.is_empty() {
        incomplete.push("attachment_contents_not_included");
    }
    if has_pending(library, "notes", &note.guid)? {
        incomplete.push("local_changes_not_sent");
    }
    let edit_text = !note.locked && note.editable && body.is_some();
    Ok(json!({
        "attachments": list,
        "incomplete_reads": incomplete,
        "edit_capabilities": {
            "edit_text": edit_text,
            "edit_formatting": edit_text && formatting.is_none(),
            "keep_attachments": true,
            "add_attachments": false,
            "remove_attachments": false,
            "edit_tables": false,
            "markdown_body": false,
            "conflict_check": "base_etag",
        },
    }))
}

/// Whether a change to this item waits to be sent. Read straight from the queue, without the
/// send's limit on how many changes go at once: a long queue must not stop a read.
fn has_pending(library: &Library, service: &str, guid: &str) -> Result<bool, &'static str> {
    Ok(library
        .outbox_pending(service)
        .map_err(|_| "library_unavailable")?
        .iter()
        .any(|p| p.record == guid))
}

/// Refuses a note save that would lose or invent structure the text cannot carry: formatting
/// that does not fit the text, a Markdown copy written over the rich original, or an
/// attachment removed, added or moved through its placeholder character.
pub(crate) fn check_note_save(
    base: &str,
    base_runs: Option<&[Run]>,
    body: &str,
    runs: Option<&[Run]>,
) -> Result<(), &'static str> {
    if runs.is_some_and(|r| !describes(body, r)) {
        return Err("runs_do_not_match_body");
    }
    let base_runs = base_runs.filter(|r| describes(base, r));
    if from_markdown_export(base, base_runs, body) {
        return Err("lossy_markdown_overwrite");
    }
    let count = |text: &str| text.chars().filter(|&c| c == ATTACHMENT).count();
    match count(body).cmp(&count(base)) {
        std::cmp::Ordering::Less => return Err("unsupported_attachment_removal"),
        std::cmp::Ordering::Greater => return Err("unsupported_attachment_insertion"),
        std::cmp::Ordering::Equal => {}
    }
    keeps_attachment_marks(base, body)?;
    if let Some(runs) = runs {
        let known: Vec<&str> = base_runs
            .map(|r| attachment_ids(r).collect())
            .unwrap_or_default();
        if attachment_ids(runs).any(|id| !known.contains(&id)) {
            return Err("unsupported_attachment_insertion");
        }
    }
    Ok(())
}

/// Refuses a save whose edits, worked out as the send works them out, take out an
/// attachment's mark: its reference goes with it, whatever mark is typed elsewhere.
pub(crate) fn keeps_attachment_marks(base: &str, body: &str) -> Result<(), &'static str> {
    let marks: Vec<u64> = attachments(base, None)
        .into_iter()
        .map(|(at, _)| at)
        .collect();
    if quince_icloud::ckdocument::utf16_hunks(base, body)
        .iter()
        .any(|&(from, removed, _)| marks.iter().any(|&m| m >= from && m < from + removed))
    {
        return Err("unsupported_attachment_move");
    }
    Ok(())
}

/// The text a save of this note will be sent as an edit of, when it differs from the cached
/// copy: saves waiting to go become one, sent from the text the first was typed over, and a
/// save after one already on its way is sent over what that one sends.
pub(crate) fn send_base(library: &Library, guid: &str) -> Result<Option<String>, &'static str> {
    let waiting = library
        .outbox_waiting("notes", guid, "save")
        .map_err(|_| "library_unavailable")?;
    let Some(last) = waiting.last() else {
        return Ok(None);
    };
    let save: quince_sync::notes::NoteSave =
        serde_json::from_str(&last.payload).map_err(|_| "invalid_cached_save")?;
    Ok(if last.tries == 0 {
        save.base
    } else {
        Some(save.body)
    })
}

/// Whether `body` is the note's Markdown export, whole or edited, rather than its own text:
/// its lines follow the export more closely than they follow the note. A line of Markdown
/// typed into the note's own text keeps it closer to the note, and is accepted as text.
fn from_markdown_export(base: &str, base_runs: Option<&[Run]>, body: &str) -> bool {
    let markdown = quince_icloud::markdown::note_to_markdown(base, base_runs.unwrap_or(&[]));
    let lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    };
    let (own, export, sent) = (lines(base), lines(&markdown), lines(body));
    if export == own {
        return false;
    }
    common_lines(&sent, &export) > common_lines(&sent, &own)
}

/// How many lines `a` and `b` have in common in the same order (longest common subsequence).
fn common_lines(a: &[String], b: &[String]) -> usize {
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diagonal = 0;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if x == y {
                diagonal + 1
            } else {
                above.max(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

/// What the send does to the reminder's date alarm for this patch: a new date moves it, no
/// date or an all-day date removes it. Alarms of other kinds, such as a place, are kept.
pub(crate) fn reminder_alarm_effect(fields: &Map<String, Value>) -> &'static str {
    if fields.get("all_day") == Some(&Value::Bool(true))
        || fields.get("due_at") == Some(&Value::Null)
    {
        "date_alarm_removed"
    } else if fields.contains_key("due_at") {
        "date_alarm_moved_to_due_date"
    } else {
        "unchanged"
    }
}

/// Refuses a reminder patch that the send would ignore or that would end a repeat.
pub(crate) fn check_reminder_edit(
    reminder: &Reminder,
    fields: &Map<String, Value>,
) -> Result<(), &'static str> {
    if fields.contains_key("all_day") && !fields.contains_key("due_at") {
        return Err("all_day_requires_due_at");
    }
    if reminder.repeats() {
        // Completed by edit, Apple marks the series done; a tick moves it to its next date.
        if fields.get("completed") == Some(&Value::Bool(true)) {
            return Err("repeating_reminder_complete_with_tick");
        }
        if fields.get("due_at") == Some(&Value::Null) {
            return Err("repeating_reminder_needs_due_date");
        }
    }
    Ok(())
}

/// The fields `reminders_get` adds: the repeat, what the cache does not carry, and which
/// edits are supported.
pub(crate) fn reminder_report(
    library: &Library,
    reminder: &Reminder,
) -> Result<Value, &'static str> {
    let recurrence = if reminder.repeats() {
        json!({
            "frequency": reminder.recur_frequency,
            "interval": reminder.recur_interval,
            "days": library.reminder_repeat_days(&reminder.guid).map_err(|_| "library_unavailable")?,
            "end": library.reminder_repeat_end(&reminder.guid).map_err(|_| "library_unavailable")?,
        })
    } else {
        Value::Null
    };
    let mut incomplete = Vec::new();
    if reminder.etag.is_none() {
        incomplete.push("not_yet_read_from_apple");
    }
    if has_pending(library, "reminders", &reminder.guid)? {
        incomplete.push("local_changes_not_sent");
    }
    Ok(json!({
        "recurrence": recurrence,
        "not_represented": ["alarms", "attachment_contents", "assignment"],
        "incomplete_reads": incomplete,
        "edit_capabilities": {
            "edit_fields": REMINDER_FIELDS,
            "conflict_check": "base_etag",
            "complete_repeating_by_edit": false,
            "clear_date_of_repeating": false,
        },
    }))
}
