//! Only Notes and Reminders run here. Reuse upstream passes and queue/crash semantics.
use quince_auth::{Client, Endpoints};
use quince_core::Library;
use quince_sync::{library_lock::LibraryLock, pass, queue};
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::Mutex;

pub(crate) const MAX_PENDING: usize = 128;
/// Refused rows kept for the human before admission stops.
pub const MAX_REFUSED: usize = 1024;

pub(crate) fn pending_operations(
    library: &Library,
) -> Result<Vec<quince_core::outbox::Pending>, &'static str> {
    // Refused rows wait for the human (`icloud-mcp quarantine`); they never count here.
    let all = library.outbox_count().map_err(|_| "library_unavailable")?;
    let refused = library
        .outbox_refused_count()
        .map_err(|_| "library_unavailable")?;
    if all.saturating_sub(refused) > MAX_PENDING {
        return Err("queue_too_large");
    }
    let mut pending = library
        .outbox_pending("notes")
        .map_err(|_| "library_unavailable")?;
    pending.extend(
        library
            .outbox_pending("reminders")
            .map_err(|_| "library_unavailable")?,
    );
    if pending.iter().any(|p| p.payload.len() > 262144) {
        return Err("payload_too_large");
    }
    Ok(pending)
}

fn pending_value(p: &quince_core::outbox::Pending) -> Value {
    // Tries changes on every retry. All operation-defining bytes must stay exact.
    json!({"id":p.id,"service":p.service,"record":p.record,"kind":p.kind,"payload":p.payload})
}

fn send_context(
    library: &Library,
    p: &quince_core::outbox::Pending,
) -> Result<Value, &'static str> {
    match (p.service.as_str(), p.kind.as_str()) {
        ("notes", "create") => {
            let template = library
                .state(quince_icloud::notes::TEMPLATE_KEY)
                .map_err(|_| "library_unavailable")?;
            if template.as_ref().is_some_and(|t| t.len() > 131072) {
                return Err("template_too_large");
            }
            Ok(json!({"template":template}))
        }
        ("reminders", "create") | (_, "delete") => Ok(Value::Null),
        // Only the cached rows the native sender reads for this operation. Lists, folders
        // and sibling items stay out, so unrelated refreshes and writes cannot stale it.
        ("notes", kind) => {
            let mut item = library
                .note(&p.record)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            // `Note.runs` never serializes; the sender takes runs from the payload. A save is
            // also checked against Apple's own copy just before it is sent
            // ([`note_changed_on_icloud`]): each sync lays a waiting save over the copy it
            // reads, so the cache can't show a change on iCloud to the saved note.
            if kind == "save" {
                item.body = library
                    .note_body(&p.record)
                    .map_err(|_| "library_unavailable")?;
            }
            bounded_context(json!({"item":crate::bounded_json(&item)?}))
        }
        ("reminders", kind) => {
            let item = library
                .reminder(&p.record)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            // An edit that moves lists also moves the cached subtasks with it.
            let edit: Value = serde_json::from_str(&p.payload).unwrap_or(Value::Null);
            let moves = kind == "edit"
                && edit["list_guid"]
                    .as_str()
                    .is_some_and(|to| Some(to) != edit["from_list"].as_str());
            let children = if moves {
                crate::bounded_json(
                    &library
                        .subtasks_of(&p.record)
                        .map_err(|_| "library_unavailable")?,
                )?
            } else {
                Value::Null
            };
            bounded_context(json!({"item":crate::bounded_json(&item)?,"children":children}))
        }
        _ => Err("unknown_service"),
    }
}

/// The container and zone Apple keeps notes in, as the native sender names them.
const NOTES_CONTAINER: &str = "com.apple.notes";
const NOTES_ZONE: &str = "Notes";

/// Whether Apple's copy of a note moved on since a waiting save was typed: the note is gone,
/// its text is not the text the save was typed over, or, for a save that sets formatting
/// of its own, Apple's formatting is not the formatting it was typed over (compared as
/// `quince_icloud::notes::same_formatting` does, so the same formatting cut into other runs
/// is no change). Apple already holding the save's own text, and its own formatting when it
/// sets any, is no change either: a send that landed but whose reply was lost. A text-only
/// save writes its text into Apple's current document and keeps
/// Apple's formatting, so its formatting is not compared; nor is formatting the save was
/// typed over that does not describe its text (the cache can keep the runs of an earlier
/// text). Read just before the send, after the sync's own read, so a change between that
/// read and this check is caught too. An answer that can't be read is left to the sender,
/// which refuses it itself.
/// `client` reads the notes container ([`NOTES_CONTAINER`]).
async fn note_changed_on_icloud(
    client: quince_icloud::cloudkit::ZoneClient<'_>,
    p: &quince_core::outbox::Pending,
) -> Result<bool, quince_icloud::Error> {
    use quince_icloud::ckdocument as doc;
    let Ok(save) = serde_json::from_str::<quince_sync::notes::NoteSave>(&p.payload) else {
        return Ok(false);
    };
    let Some(current) = client.lookup_record(NOTES_ZONE, &p.record).await? else {
        return Ok(true);
    };
    let Some(encoded) = current
        .pointer("/fields/TextDataEncrypted/value")
        .and_then(Value::as_str)
    else {
        return Ok(false);
    };
    let encoded = doc::with_apple_line_breaks_in_base64(encoded);
    let (Some(theirs), Some(base)) = (doc::stored_text_from_base64(&encoded), save.base) else {
        return Ok(false);
    };
    // Apple already holds this save (a send that landed, its reply lost): no change on
    // iCloud. The sender then finds nothing to write and finishes quietly.
    let saved = doc::apple_line_breaks(&save.body);
    let holds_formatting = match &save.runs {
        None => true,
        Some(runs) => quince_icloud::notes::same_formatting(
            &saved,
            &doc::runs_with_apple_line_breaks(&save.body, runs.clone()),
            &doc::runs_from_base64(&encoded),
        ),
    };
    if saved == theirs && holds_formatting {
        return Ok(false);
    }
    let typed_over = doc::apple_line_breaks(&base);
    if typed_over != theirs {
        return Ok(true);
    }
    if let (Some(_), Some(base_runs)) = (save.runs, save.base_runs) {
        let base_runs = doc::runs_with_apple_line_breaks(&base, base_runs);
        if quince_icloud::notes::canonical_runs(&typed_over, &base_runs).is_some()
            && !quince_icloud::notes::same_formatting(
                &typed_over,
                &base_runs,
                &doc::runs_from_base64(&encoded),
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn bounded_context(context: Value) -> Result<Value, &'static str> {
    crate::bounded_value(&context, crate::MAX_ARGUMENT_BYTES)?;
    Ok(without_version_stamps(context))
}

/// A context without the etag and modified time of its items. Apple gives a note or reminder
/// a new etag (and time) when it next reads it back, even one just made, with nothing else
/// changed, so neither is compared. What is compared is the item as the cache showed it when
/// the write was queued and as it shows it after this sync's read from Apple. The senders
/// then write with an etag: some send the cached one, and CloudKit refuses the write if
/// Apple's record changed since that read; others look the record up first and write over
/// what Apple holds then. A change made on another device between this sync's read and the
/// send is not caught here, except for a note save, which is checked against Apple's copy
/// just before it is sent ([`note_changed_on_icloud`]).
fn without_version_stamps(mut context: Value) -> Value {
    fn strip(item: &mut Value) {
        if let Some(item) = item.as_object_mut() {
            item.remove("etag");
            item.remove("modifiedAt");
        }
    }
    if let Some(context) = context.as_object_mut() {
        if let Some(item) = context.get_mut("item") {
            strip(item);
        }
        if let Some(children) = context.get_mut("children").and_then(Value::as_array_mut) {
            children.iter_mut().for_each(strip);
        }
    }
    context
}

/// The item a queued write is bound to changed on iCloud (or was deleted there) after the
/// write was queued, so the write is never sent.
pub(crate) const ITEM_CHANGED: &str = "item_changed_on_icloud_since_queued";

/// The next step after [`ITEM_CHANGED`].
/// It names [`MAX_REFUSED`]; a test holds the two together.
pub(crate) const ITEM_CHANGED_STEP: &str = "The note or reminder changed on iCloud after this change was queued, or was deleted there, so the change was not sent. If it still exists, read it again with notes_get or reminders_get and ask again. If it was deleted there is nothing to redo. The set-aside change stays in the list, and counts toward the limit of 1024 set-aside changes, until you retire it with icloud-mcp quarantine in your own terminal.";

/// Grant failures that belong to one queued write. Anything else (an unreadable library,
/// a changed policy, account or directory) still stops the whole pass.
const QUARANTINABLE: [&str; 13] = [
    "payload_too_large",
    "preview_too_large",
    "invalid_preview",
    "legacy_write_quarantined",
    "authorization_too_large",
    "invalid_authorization",
    "write_authorization_stale",
    ITEM_CHANGED,
    "write_mode_stricter",
    "not_found",
    "operation_too_large",
    "template_too_large",
    "unknown_service",
];

/// Moves each pending write without a current grant out of the queue, alone, with its
/// reason. It is kept for the human and never sent or re-authorized.
fn quarantine_unauthorized(library: &mut Library, dir: &Path) -> Result<(), &'static str> {
    let pending = library
        .outbox_all_pending()
        .map_err(|_| "library_unavailable")?;
    for p in pending {
        match authorized(library, dir, &p) {
            Ok(()) => {}
            Err(reason) if QUARANTINABLE.contains(&reason) => library
                .atomically(|library| -> quince_core::Result<()> {
                    library.outbox_refuse(p.id, &format!("mcp_quarantine:{reason}"))?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    library.set_state(&reread_key(&p.service), "1")
                })
                .map_err(|_| "library_unavailable")?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Set when a write is quarantined: the cache still shows it as if on its way. The next
/// pass that reaches the drain re-reads the service in full, which replays only waiting
/// writes, and clears this once that pass succeeds. Kept until then, across failures.
pub(crate) fn reread_key(service: &str) -> String {
    format!("mcp.full_reread.{service}")
}

/// What `cloudkit_accepted` in the sync reply does and does not say.
const LEFT_QUEUE_NOTE: &str = "cloudkit_accepted: CloudKit took every write the sender made. A reminder's repeat or alert is left out on purpose when its date was cleared, moved past the repeat's last day or made all-day on another device, or a repeat was set there meanwhile. The list-order write is not checked.";

/// Whether this write's sender answers done only when CloudKit took every write it makes:
/// every reminder create. The create, its date or flag, and a repeat's or alert's every step
/// each fail the change, which stays queued (or is refused) until Apple takes them all. Other
/// senders can skip work, so they are not called accepted.
fn every_write_checked(p: &quince_core::outbox::Pending) -> bool {
    p.service == "reminders"
        && p.kind == "create"
        && serde_json::from_str::<quince_sync::reminders::Create>(&p.payload).is_ok()
}

/// Where the last sync noted why a service's queue stopped: the change it stopped at (none
/// when the refresh before sending failed), a fixed reason code and that change's try.
fn waiting_key(service: &str) -> String {
    format!("mcp.waiting.{service}")
}

/// Why a change Apple did not take this time waits, as a fixed code. Never Apple's words,
/// which can hold HTTP bodies.
fn waiting_reason(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::ReauthRequired | E::NeedsDeviceApproval => "waiting_sign_in",
        E::Busy(_) | E::StaleCursor => "waiting_apple_busy",
        E::Protocol(_) => "waiting_answer_not_understood",
        E::Conflict => "waiting_changed_at_apple",
        E::Io(_) | E::Storage(_) => "waiting_local_storage",
        _ => "waiting_for_connection",
    }
}

/// Why nothing was sent when the refresh before sending did not finish.
fn refresh_reason(outcome: pass::Outcome) -> &'static str {
    match outcome {
        pass::Outcome::Paused => "waiting_sign_in",
        pass::Outcome::Unavailable | pass::Outcome::Preparing => "waiting_apple_busy",
        _ => "waiting_refresh_failed",
    }
}

/// How many waiting changes a reply lists, oldest first: the first is the one the queue
/// stopped at, and every later one waits behind it.
const PENDING_SHOWN: usize = 20;

/// What to do about a change waiting for `reason`, said to the person. `None` for a code
/// that is not a waiting reason.
pub(crate) fn waiting_step(reason: &str, dir: &Path) -> Option<String> {
    if let Some(first) = reason.strip_prefix("waiting_behind_change_") {
        let first: i64 = first.parse().ok()?;
        return Some(format!("This change was not sent yet: changes go to Apple in order, and it waits behind change {first}. It goes once change {first} is sent or refused; see the reason given for change {first}."));
    }
    Some(match reason {
        "waiting_for_connection" => "Apple couldn't be reached, or the connection dropped. Nothing to do: the next sync sends it again.".to_string(),
        "waiting_apple_busy" => "Apple was busy or not ready. Nothing to do: the next sync sends it again.".to_string(),
        "waiting_sign_in" => format!("The sign-in has to be renewed, or Apple is waiting for approval on one of your devices. Sign in with {} in a terminal you opened yourself, approve on your device if it asks, then run sync again.", crate::status::with_dir("icloud-mcp login", dir)),
        "waiting_answer_not_understood" => "Apple's answer couldn't be read. Wait a few minutes, then run sync again: it tries the change again, up to 5 times in all, and then holds it as refused.".to_string(),
        "waiting_changed_at_apple" => "The item changed in iCloud since this change was queued. Run sync again: it reads the item again and tries the change, up to 5 times in all, and then holds it as refused. If it is refused, read the item again with notes_get or reminders_get and make the change again.".to_string(),
        "waiting_local_storage" => format!("iCloud-MCP couldn't read or write its own files. In your own terminal, run {} to check this folder, then run sync again.", crate::status::with_dir("icloud-mcp status", dir)),
        "waiting_refresh_failed" => "The refresh before sending failed, so nothing was sent. Run sync again in a few minutes.".to_string(),
        "waiting_to_send" => "No sync has tried to send it yet. Run sync to send it.".to_string(),
        _ => return None,
    })
}

/// The oldest waiting changes of `service` as MCP may show them: identifiers, why each waits
/// and what to do, never the payload or Apple's words. The change the queue stopped at for
/// an answer not understood or a record changed at Apple also says which try it was of the
/// tries allowed; the changes after it wait behind it and were not sent.
pub(crate) fn pending_view(
    library: &Library,
    dir: &Path,
    service: &str,
) -> Result<Vec<Value>, &'static str> {
    let noted: Option<Value> = library
        .state(&waiting_key(service))
        .map_err(|_| "library_unavailable")?
        .and_then(|raw| serde_json::from_str(&raw).ok());
    Ok(library
        .outbox_pending(service)
        .map_err(|_| "library_unavailable")?
        .iter()
        .take(PENDING_SHOWN)
        .map(|p| {
            let mut reason = "waiting_to_send".to_string();
            let mut tried = None;
            if let Some(noted) = &noted {
                let said = noted["reason"].as_str().unwrap_or("waiting_to_send");
                match noted["id"].as_i64() {
                    // The refresh failed: nothing was sent, each waits for the same reason.
                    None => reason = said.to_string(),
                    Some(at) if p.id == at => {
                        reason = said.to_string();
                        if matches!(
                            said,
                            "waiting_answer_not_understood" | "waiting_changed_at_apple"
                        ) {
                            tried = noted["try"].as_i64();
                        }
                    }
                    Some(at) if p.id > at => reason = format!("waiting_behind_change_{at}"),
                    Some(_) => {}
                }
            }
            let mut view = json!({"id":p.id,"service":p.service,"kind":p.kind,"record":p.record,
                "reason":reason,"next_step":waiting_step(&reason, dir)});
            if let Some(tried) = tried {
                view["try"] = json!(tried);
                view["tries_allowed"] = json!(quince_sync::queue::PROTOCOL_TRIES);
            }
            view
        })
        .collect())
}

/// Two services' waiting lists as one, oldest first, cut to [`PENDING_SHOWN`].
fn oldest_first(mut first: Vec<Value>, second: Vec<Value>) -> Vec<Value> {
    first.extend(second);
    first.sort_by_key(|v| v["id"].as_i64());
    first.truncate(PENDING_SHOWN);
    first
}

/// Our own refusal codes are shown; engine and Apple reasons can hold HTTP text.
fn shown_reason(reason: &str) -> &str {
    if reason.starts_with("mcp_quarantine:")
        || reason == "mcp_policy_migration_quarantine"
        || reason == "superseded_by_approved_deletion"
    {
        reason
    } else {
        "refused_by_sync_engine"
    }
}

/// A refused write as MCP may show it: identifiers and a reason code, never the payload.
pub(crate) fn refused_view(r: &quince_core::outbox::RefusedBrief) -> Value {
    let mut view = json!({"id":r.id,"service":r.service,"kind":r.kind,"record":r.record,"reason":shown_reason(&r.reason)});
    if r.reason.strip_prefix("mcp_quarantine:") == Some(ITEM_CHANGED) {
        view["instructions"] = json!(ITEM_CHANGED_STEP);
    }
    view
}

fn refused_ids(dir: &Path) -> Result<Vec<i64>, &'static str> {
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    Ok(library
        .outbox_refused_brief()
        .map_err(|_| "library_unavailable")?
        .into_iter()
        .map(|r| r.id)
        .collect())
}

/// Records the grant a queued write is sent under: the stored policy, the approval mode in
/// force when it was queued (the file's, else the stored one), the write and its context.
/// `mode` is the mode the call's gate decided with; it is not read again here, so a file
/// changed meanwhile can't record a write as made under a stricter mode than it was.
pub(crate) fn record_authorization(
    library: &mut Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
    mode: Option<&str>,
) -> Result<(), &'static str> {
    let policy = library
        .state("mcp.policy")
        .map_err(|_| "policy_unavailable")?
        .ok_or("policy_unavailable")?;
    // No approval mode in force (a folder back to its setup choice) is recorded as "none".
    let mode = match mode {
        Some(mode) => mode.to_string(),
        None => crate::mode_in_force(library, dir)?.unwrap_or_else(|| "none".into()),
    };
    let grant = json!({"policy":policy,"mode":mode,"pending":pending_value(p),
        "context":send_context(library,p)?});
    library
        .set_state(&format!("mcp.authorization.{}", p.id), &grant.to_string())
        .map_err(|_| "library_unavailable")
}

/// Testable send boundary. No tool can create or change this authorization.
pub fn authorized(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
) -> Result<(), &'static str> {
    authorization_check(library, dir, p, true)
}

pub(crate) fn authorized_payload(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
) -> Result<(), &'static str> {
    authorization_check(library, dir, p, false)
}

fn authorization_check(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
    context: bool,
) -> Result<(), &'static str> {
    let Some((stored, binding)) = crate::policy(library, dir)? else {
        return Err("configured_policy_required");
    };
    if p.payload.len() > 262144 {
        return Err("payload_too_large");
    }
    let raw = library
        .state(&format!("mcp.authorization.{}", p.id))
        .map_err(|_| "library_unavailable")?
        .ok_or("legacy_write_quarantined")?;
    if raw.len() > 1572864 {
        return Err("authorization_too_large");
    }
    let grant: Value = serde_json::from_str(&raw).map_err(|_| "invalid_authorization")?;
    // A grant from before the mode was recorded has three fields and was made under the
    // stored mode.
    let fields = grant.as_object().map_or(0, |m| m.len());
    if !(fields == 3 || (fields == 4 && grant["mode"].is_string()))
        || grant["policy"] != binding.policy
        || grant["pending"] != pending_value(p)
    {
        return Err("write_authorization_stale");
    }
    // The item itself changed, or is gone, since the write was queued. A grant recorded
    // before the etag and time were left out is compared without them too. A create's
    // context holds no item; a change there is a stale grant.
    if context {
        let mut queued = without_version_stamps(grant["context"].clone());
        let mut now = match send_context(library, p) {
            Err("not_found") => return Err(ITEM_CHANGED),
            now => now?,
        };
        // A cached formatting value, recorded by a build before the note save was checked
        // against Apple's copy, means nothing now: Apple's copy is compared instead.
        for context in [&mut queued, &mut now] {
            if let Some(context) = context.as_object_mut() {
                context.remove("runs");
            }
        }
        if queued != now {
            return Err(if queued.get("item").is_some() {
                ITEM_CHANGED
            } else {
                "write_authorization_stale"
            });
        }
    }
    // Queued under a looser mode than the one in force now: never sent.
    let queued = grant["mode"].as_str().unwrap_or(&stored.mode).to_string();
    let queued = (queued != "none").then_some(queued);
    let now = crate::mode_in_force(library, dir)?;
    if crate::permissions::mode_rank(now.as_deref(), false)
        > crate::permissions::mode_rank(queued.as_deref(), false)
    {
        return Err("write_mode_stricter");
    }
    Ok(())
}

/// Sets aside every queued write made under a looser approval mode than the one in force,
/// so it is never sent and the next write is not blocked by it.
pub(crate) fn quarantine_looser(library: &mut Library, dir: &Path) -> Result<(), &'static str> {
    if crate::policy(library, dir)?.is_none() {
        return Ok(());
    }
    for p in library
        .outbox_all_pending()
        .map_err(|_| "library_unavailable")?
    {
        if authorization_check(library, dir, &p, false) == Err("write_mode_stricter") {
            library
                .atomically(|library| -> quince_core::Result<()> {
                    library.outbox_refuse(p.id, "mcp_quarantine:write_mode_stricter")?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    library.set_state(&reread_key(&p.service), "1")
                })
                .map_err(|_| "library_unavailable")?;
        }
    }
    Ok(())
}

/// Notes and Reminders, as far as the permissions file's `services:` line leaves them on.
fn services_on(library: &Library, dir: &Path) -> Result<Vec<&'static str>, &'static str> {
    let on = crate::permissions::peek(library, dir, None)?.services;
    Ok(["notes", "reminders"]
        .into_iter()
        .filter(|s| on.contains(s))
        .collect())
}

/// Sets aside every queued write of Notes or Reminders while that service is switched off,
/// as [`quarantine_looser`] does: it is never sent, also not after the service is switched
/// on again, and `icloud-mcp quarantine` and `sync_status` count it.
pub(crate) fn set_aside_switched_off(
    library: &mut Library,
    on: &[&str],
) -> Result<(), &'static str> {
    for service in ["notes", "reminders"]
        .into_iter()
        .filter(|s| !on.contains(s))
    {
        for p in library
            .outbox_pending(service)
            .map_err(|_| "library_unavailable")?
        {
            library
                .atomically(|library| -> quince_core::Result<()> {
                    library.outbox_refuse(p.id, "mcp_quarantine:service_switched_off")?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    library.set_state(&reread_key(service), "1")
                })
                .map_err(|_| "library_unavailable")?;
        }
    }
    Ok(())
}

pub async fn run(dir: &Path) -> Result<Value, &'static str> {
    run_authorized(dir, false).await
}

fn refresh_gate(dir: &Path, approved: bool) -> Result<bool, &'static str> {
    let mut library = Library::open(dir).map_err(|_| "library_unavailable")?;
    let configured = crate::policy(&library, dir)?;
    let on = services_on(&library, dir)?;
    set_aside_switched_off(&mut library, &on)?;
    if on.is_empty() {
        return Err("sync_switched_off");
    }
    if crate::mode_in_force(&library, dir)?.as_deref() == Some("reads_and_writes") && !approved {
        return Err("read_approval_required");
    }
    if configured.is_some() {
        quarantine_unauthorized(&mut library, dir)?;
    }
    Ok(configured.is_some())
}

pub(crate) async fn run_authorized(dir: &Path, approved: bool) -> Result<Value, &'static str> {
    let before = refused_ids(dir)?;
    refresh_gate(dir, approved)?;
    let store = crate::storage::open_store(dir)?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    let client = Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")?;
    with_client_inner(dir, client, approved, before).await
}

/// Test seam with synthetic endpoints. Production constructs its own Apple-only client.
/// Caller holds the private directory's exclusive operation lock.
pub async fn with_client(dir: &Path, client: Client) -> Result<Value, &'static str> {
    let before = refused_ids(dir)?;
    with_client_inner(dir, client, false, before).await
}

/// `before` holds the refused ids from before this sync's first gate, so the reply can
/// name every write this sync refused or quarantined.
async fn with_client_inner(
    dir: &Path,
    client: Client,
    approved: bool,
    before: Vec<i64>,
) -> Result<Value, &'static str> {
    let configured = refresh_gate(dir, approved)?;
    if configured {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            sync_pass(dir, client, true, &before),
        )
        .await
        .map_err(|_| "sync_timeout_delivery_uncertain")?
    } else {
        sync_pass(dir, client, false, &before).await
    }
}

async fn sync_pass(
    dir: &Path,
    mut client: Client,
    configured: bool,
    before_refused: &[i64],
) -> Result<Value, &'static str> {
    if !client
        .resume()
        .await
        .map_err(|_| "login_required_or_network_unavailable")?
    {
        return Err("login_required");
    }
    if client.waits_on_icloud_com() {
        return Err("login_required_complete_account_setup");
    }
    let dsid = client.session().dsid.as_deref().ok_or("login_required")?;
    let tag = queue::account_tag(dsid);
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    let on = services_on(&library, dir)?;
    if crate::account::binding(&library, dsid)? != crate::account::Binding::Same {
        return Err("account_mismatch_use_new_state_directory");
    }
    let data = LibraryLock::new(library, Some(dir.to_owned()));
    let ctx = pass::Ctx::new(
        client.http().clone(),
        client.service_params(),
        client.session().service_url("ckdatabasews"),
        client.data_access_missing(),
        &pass::NoHooks,
    );
    let root = ctx.cloudkit.as_deref().ok_or("service_unavailable")?;
    let mut services = Vec::new();
    // Each write waiting before the drain: the ones gone after it, not refused, left the
    // queue. Only some senders answer done only when CloudKit took every write; the rest can
    // finish with a part logged and skipped, or with nothing written (a tick another device
    // overtook), so leaving the queue is reported as accepted only for those. Neither is read
    // back.
    let mut waiting_before = Vec::new();
    for service in ["notes", "reminders"] {
        // Switched off in permissions.yaml: nothing is read. Its queued changes were set
        // aside in refresh_gate and are never sent.
        if !on.contains(&service) {
            services.push(json!({"service":service,"outcome":"switched_off","sent":false}));
            continue;
        }
        let lock = Mutex::new(());
        let before = pass::run(service, &ctx, &data, &lock).await;
        // Each sync says afresh why its queue stopped, if it did.
        let noted = if matches!(before.outcome, pass::Outcome::Ok) {
            String::new()
        } else {
            json!({"id":null,"reason":refresh_reason(before.outcome)}).to_string()
        };
        data.lock()
            .await
            .set_state(&waiting_key(service), &noted)
            .map_err(|_| "library_unavailable")?;
        waiting_before.extend(
            data.lock()
                .await
                .outbox_pending(service)
                .map_err(|_| "library_unavailable")?,
        );
        // Never send when the authenticated read is gated or failed.
        let mut sent = false;
        if matches!(before.outcome, pass::Outcome::Ok) {
            let report = queue::drain_as(&data, service, &lock, &tag, |p| {
                let data = &data;
                let http = &ctx.http;
                let params = ctx.params.clone();
                async move {
                    // The drain refuses a row set aside here; its grant is emptied as before
                    // the pass, so it can never be sent under this grant again.
                    let set_aside = |reason: &str| {
                        let reason = format!("mcp_quarantine:{reason}");
                        let id = p.id;
                        async move {
                            let mut library = data.lock().await;
                            let _ = library.set_state(&format!("mcp.authorization.{id}"), "");
                            let _ = library.set_state(&reread_key(service), "1");
                            quince_icloud::Error::Refused(reason)
                        }
                    };
                    if configured {
                        let refused = authorized(&*data.lock().await, dir, &p).err();
                        if let Some(reason) = refused {
                            return Err(set_aside(reason).await);
                        }
                    }
                    let (id, tries) = (p.id, p.tries);
                    // A note save is also checked against Apple's copy, read now.
                    let checked = if configured && service == "notes" && p.kind == "save" {
                        let client = quince_icloud::cloudkit::ZoneClient::new(
                            http,
                            root,
                            NOTES_CONTAINER,
                            params.clone(),
                        );
                        note_changed_on_icloud(client, &p).await
                    } else {
                        Ok(false)
                    };
                    let sent = match checked {
                        Ok(true) => return Err(set_aside(ITEM_CHANGED).await),
                        Err(e) => Err(e),
                        Ok(false) if service == "notes" => {
                            quince_sync::notes::send(data, http, root, params, p).await
                        }
                        Ok(false) => {
                            quince_sync::reminders::send(data, http, root, params, p).await
                        }
                    };
                    // Kept queued (the queue's own rule): note why, as a code, for the reply.
                    if let Err(e) = &sent {
                        if quince_sync::queue::transient(e, tries) {
                            let noted = json!({"id":id,"reason":waiting_reason(e),"try":tries});
                            let _ = data
                                .lock()
                                .await
                                .set_state(&waiting_key(service), &noted.to_string());
                        }
                    }
                    sent
                }
            })
            .await;
            sent = report.sent;
        }
        let reread = configured
            && matches!(before.outcome, pass::Outcome::Ok)
            && data
                .lock()
                .await
                .state(&reread_key(service))
                .map_err(|_| "library_unavailable")?
                .as_deref()
                == Some("1");
        if reread {
            data.lock()
                .await
                .forget_full_sync(service)
                .map_err(|_| "library_unavailable")?;
        }
        let after = if sent || reread {
            pass::run(service, &ctx, &data, &lock).await
        } else {
            before
        };
        if reread && matches!(after.outcome, pass::Outcome::Ok) {
            data.lock()
                .await
                .set_state(&reread_key(service), "")
                .map_err(|_| "library_unavailable")?;
        }
        // Raw upstream error details can contain HTTP bodies. Expose only typed outcomes.
        services.push(json!({"service":service,"outcome":after.outcome,"sent":sent,"last_synced_at":after.last_synced_at}));
    }
    let library = data.lock().await;
    let refused = library
        .outbox_refused_brief()
        .map_err(|_| "library_unavailable")?;
    let waiting = library
        .outbox_count()
        .map_err(|_| "library_unavailable")?
        .saturating_sub(refused.len());
    let mut still = Vec::new();
    for service in ["notes", "reminders"] {
        still.extend(
            library
                .outbox_pending(service)
                .map_err(|_| "library_unavailable")?
                .into_iter()
                .map(|p| p.id),
        );
    }
    let left: Vec<Value> = waiting_before
        .iter()
        .filter(|p| !still.contains(&p.id) && !refused.iter().any(|r| r.id == p.id))
        .take(MAX_PENDING)
        .map(|p| {
            let outcome = if every_write_checked(p) {
                "cloudkit_accepted"
            } else {
                "finished_unconfirmed"
            };
            json!({"service":p.service,"kind":p.kind,"record":p.record,"outcome":outcome})
        })
        .collect();
    // Bounded: admission stops at MAX_PENDING waiting writes, and each is refused at most once.
    let refused_now: Vec<Value> = refused
        .iter()
        .filter(|r| !before_refused.contains(&r.id))
        .take(MAX_PENDING)
        .map(refused_view)
        .collect();
    let pending_recent = oldest_first(
        pending_view(&library, dir, "notes")?,
        pending_view(&library, dir, "reminders")?,
    );
    let mut reply = json!({"services":services,"queued":waiting,"pending_recent":pending_recent,"refused":refused.len(),"refused_this_sync":refused_now,"left_queue_this_sync":left,"remote_verified":false,"verification":"upstream sync and replay only; field-level read-back is not guaranteed"});
    // Said only when there is something for it to explain.
    if !left.is_empty() {
        reply["left_queue_note"] = json!(LEFT_QUEUE_NOTE);
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each reason Apple's error can give is its own code, and a record changed at Apple is
    /// not said as an answer that couldn't be read.
    #[test]
    fn each_error_waits_with_its_own_reason() {
        use quince_icloud::Error as E;
        assert_eq!(waiting_reason(&E::Conflict), "waiting_changed_at_apple");
        assert_eq!(
            waiting_reason(&E::Protocol("synthetic".into())),
            "waiting_answer_not_understood"
        );
        assert_eq!(waiting_reason(&E::Busy(None)), "waiting_apple_busy");
        assert_eq!(waiting_reason(&E::Stalled), "waiting_for_connection");
        assert_eq!(waiting_reason(&E::ReauthRequired), "waiting_sign_in");
        assert_eq!(
            waiting_reason(&E::Io(std::io::Error::other("synthetic"))),
            "waiting_local_storage"
        );
    }

    /// Every reason has a next step, and one waiting behind names the change it waits for.
    #[test]
    fn every_waiting_reason_says_what_to_do() {
        let dir = std::path::Path::new("/synthetic/state");
        for reason in [
            "waiting_for_connection",
            "waiting_apple_busy",
            "waiting_sign_in",
            "waiting_answer_not_understood",
            "waiting_changed_at_apple",
            "waiting_local_storage",
            "waiting_refresh_failed",
            "waiting_to_send",
        ] {
            assert!(waiting_step(reason, dir).is_some(), "{reason}");
        }
        assert!(waiting_step("waiting_sign_in", dir)
            .unwrap()
            .contains("icloud-mcp login"));
        assert!(waiting_step("waiting_local_storage", dir)
            .unwrap()
            .contains("icloud-mcp status"));
        assert!(waiting_step("waiting_behind_change_42", dir)
            .unwrap()
            .contains("change 42"));
        assert!(waiting_step("waiting_behind_change_x", dir).is_none());
        assert!(waiting_step("waiting_unknown", dir).is_none());
    }

    /// The list across Notes and Reminders is the oldest changes, whichever service.
    #[test]
    fn the_waiting_list_is_oldest_first_across_services() {
        let view = |id: i64| json!({"id":id});
        let notes: Vec<Value> = (100..130).map(view).collect();
        let reminders: Vec<Value> = (1..5).map(view).collect();
        let shown = oldest_first(notes, reminders);
        assert_eq!(shown.len(), PENDING_SHOWN);
        let ids: Vec<i64> = shown.iter().map(|v| v["id"].as_i64().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        assert_eq!(&ids[..4], &[1, 2, 3, 4]);
    }
}
