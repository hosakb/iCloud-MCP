//! Local MCP adapter. The library and codecs remain owned by the upstream crates.
pub mod account;
pub mod calendar;
pub mod calendar_write;
pub mod contacts;
pub mod contacts_write;
pub mod drive;
pub mod drive_write;
pub mod exchange;
pub mod keyhold;
pub mod login;
pub mod permissions;
pub mod photos;
pub mod photos_write;
mod preserve;
pub mod scheduler;
pub mod setup;
pub mod state;
pub mod status;
pub mod storage;
pub mod sync;
pub mod terminal;
use quince_core::Library;
use quince_sync::queue::ACCOUNT_KEY;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

pub const MAX_FRAME_BYTES: usize = 65536;
const MAX_ARGUMENT_BYTES: usize = 32768;
const MAX_PREVIEW_BYTES: usize = 131072;

// Count before serialization or cloning. Also cap the shape of already-parsed
// library callers, independently of the stdio parser's recursion limit.
fn bounded_value(value: &Value, limit: usize) -> Result<(), &'static str> {
    fn visit(
        v: &Value,
        left: &mut usize,
        nodes: &mut usize,
        depth: usize,
    ) -> Result<(), &'static str> {
        if depth > 32 || *nodes == 0 {
            return Err("operation_too_large");
        }
        *nodes -= 1;
        let size = match v {
            Value::String(s) => s.len(),
            Value::Object(m) => m
                .keys()
                .try_fold(0usize, |n, k| n.checked_add(k.len()))
                .ok_or("operation_too_large")?,
            _ => 8,
        };
        *left = left.checked_sub(size).ok_or("operation_too_large")?;
        match v {
            Value::Array(a) => {
                for v in a {
                    visit(v, left, nodes, depth + 1)?;
                }
            }
            Value::Object(m) => {
                for v in m.values() {
                    visit(v, left, nodes, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut remaining = limit;
    let mut nodes = 4096;
    visit(value, &mut remaining, &mut nodes, 0)
}

/// Cancellation-safe bounded framing: the caller retains partially received bytes
/// across timer ticks. Oversized frames close the connection without parsing.
pub async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    input: &mut R,
    frame: &mut Vec<u8>,
) -> Result<Option<String>, &'static str> {
    use tokio::io::AsyncBufReadExt;
    loop {
        let available = input.fill_buf().await.map_err(|_| "transport_failed")?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(std::mem::take(frame))
                .map(Some)
                .map_err(|_| "invalid_utf8");
        }
        let end = available.iter().position(|b| *b == b'\n');
        let count = end.map_or(available.len(), |n| n + 1);
        if count > MAX_FRAME_BYTES.saturating_sub(frame.len()) {
            return Err("frame_too_large");
        }
        frame.extend_from_slice(&available[..count]);
        input.consume(count);
        if end.is_some() {
            return String::from_utf8(std::mem::take(frame))
                .map(Some)
                .map_err(|_| "invalid_utf8");
        }
    }
}

#[derive(Clone, PartialEq)]
struct ApprovalBinding {
    policy: String,
    account: Option<String>,
    identity: (u64, u64),
    snapshot: Option<Value>,
    preview_read: bool,
    /// The permissions file's fingerprint when the card was shown; any edit makes it stale.
    rules: Option<String>,
}

struct PendingConsent {
    id: String,
    request: Value,
    binding: ApprovalBinding,
    deadline: std::time::Instant,
    /// The rule name "Always allow" would save, when the card offers it.
    always: Option<String>,
}

/// How long an approval card waits for the person before it counts as Deny.
const CARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// The error for a call the permissions file sets to deny.
pub const DENIED: &str = "denied_by_permissions";

/// A tool error with a sentence for the permission codes a person has to act on.
fn plain_error(error: &str) -> Value {
    let mut value = json!({"error":error});
    let message = match error {
        DENIED => "The permissions file in Skykeep's folder sets this action to deny, so it did not run.",
        "approval_policy_changed" => "Something changed while the approval card was open (the item, the approval mode or the permissions file), so the approval no longer counts and nothing ran. Ask again to get a new card.",
        "approval_pending" => "Another action is waiting for an answer on the approval card. Answer or close that card first, then ask again.",
        "approval_requires_form_client" => "This action needs a person to approve it on the approval card in the MCP client, and this client did not offer approval cards when it connected, so nothing ran. Ask again from a client that shows the card, such as Hermes, while someone is there to answer it.",
        "approval_denied" => "The approval card was denied, declined or closed without a choice, so nothing ran.",
        "approval_cancelled" => "The request was cancelled before the approval card was answered, so nothing ran.",
        "approval_timeout" => "Nobody answered the approval card within 3 minutes, so nothing ran. A session where nobody can answer, such as a scheduled job, can't run actions that need approval. Ask again while someone can answer the card.",
        SYNC_OFF => "Notes and Reminders are both switched off for this server in permissions.yaml (services:), so sync has nothing to refresh or send, and nothing ran.",
        e => match switched_off_service(e) {
            Some(name) => return switched_off_value(value, name),
            None => return value,
        },
    };
    value["message"] = json!(message);
    value
}

/// The error for `sync` while Notes and Reminders are both switched off.
const SYNC_OFF: &str = "sync_switched_off";

/// The error for a call to a tool whose service the `services:` line switches off.
fn switched_off(tool: &str) -> &'static str {
    match permissions::service_of(tool) {
        Some("notes") => "notes_switched_off",
        Some("reminders") => "reminders_switched_off",
        Some("contacts") => "contacts_switched_off",
        Some("calendar") => "calendar_switched_off",
        Some("photos") => "photos_switched_off",
        Some("drive") => "drive_switched_off",
        _ => SYNC_OFF,
    }
}

/// The service's name for people, for a [`switched_off`] code.
fn switched_off_service(error: &str) -> Option<&'static str> {
    let service = error.strip_suffix("_switched_off")?;
    permissions::SERVICES
        .iter()
        .find(|(s, _)| *s == service)
        .map(|(_, name)| *name)
}

fn switched_off_value(mut value: Value, name: &str) -> Value {
    value["message"] = json!(format!("{name} is switched off for this server in permissions.yaml (services:), so this did not run and nothing from {name} was read."));
    value
}

/// Who decides a call, after the permissions file and the approval mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    Run,
    Client,
    Terminal,
    Deny,
    /// A folder without an approval mode and no rule: the older behaviour decides.
    Legacy,
}

/// The rule the permissions file sets for this call, or `None` when no rule applies. A Notes
/// save that empties a note, or a move into Recently Deleted, also follows the delete rule:
/// the stricter of the two decides.
fn decide(
    mode: Option<&str>,
    require_all: bool,
    rules: &permissions::Rules,
    name: &str,
    args: &Value,
) -> Option<permissions::Effect> {
    let action = args["action"].as_str();
    let own = permissions::rule_for(rules, name, action);
    if name != "notes_write"
        || action == Some("delete")
        || permissions::removal(name, args) == permissions::Removal::None
    {
        return own;
    }
    let delete = permissions::rule_for(rules, name, Some("delete"));
    if own.is_none() && delete.is_none() {
        return None;
    }
    let or_default = |effect: Option<permissions::Effect>, action: &str| {
        effect.unwrap_or_else(|| permissions::default_effect(mode, require_all, name, Some(action)))
    };
    Some(or_default(own, action.unwrap_or_default()).max(or_default(delete, "delete")))
}

fn gate_for(
    library: &Library,
    mode: Option<&str>,
    active: &permissions::Active,
    name: &str,
    args: &Value,
) -> Result<(Gate, Option<permissions::Effect>), &'static str> {
    // A switched-off service is refused before any rule, mode or card, deletions included.
    if !active.tool_on(name) {
        return Err(switched_off(name));
    }
    let require_all = library
        .state("mcp.require_write_approval")
        .map_err(|_| "library_unavailable")?
        .as_deref()
        == Some("true");
    let explicit = if permissions::ALWAYS.contains(&name) {
        None
    } else {
        decide(mode, require_all, &active.rules, name, args)
    };
    use permissions::Effect;
    let gate = match explicit {
        Some(Effect::Allow) => Gate::Run,
        Some(Effect::AskClient) => Gate::Client,
        Some(Effect::AskTerminal) => Gate::Terminal,
        Some(Effect::Deny) => Gate::Deny,
        None => match mode {
            Some(mode) if needs_consent(mode, name)? => Gate::Client,
            Some(_) => Gate::Run,
            None => {
                needs_consent("yolo", name)?;
                Gate::Legacy
            }
        },
    };
    // Deletions, trash, moves to Recently Deleted and renames or moves of large Drive folders
    // always need a person: an allow rule, yolo or a folder without an approval mode can't let
    // them run unasked. They are asked on the card in every mode; only an explicit
    // ask: terminal rule sends them to skykeep-mcp approve instead.
    let person =
        drive_needs_person(library, name, args)? || contacts_write::needs_person(name, args);
    let removes = permissions::removal(name, args) != permissions::Removal::None;
    // A folder without a mode that was set to approve every change (require-approval). Drive
    // and Calendar keep asking then even where a rule allows the change.
    let every_change = mode.is_none()
        && require_all
        && args["action"].is_string()
        && (gate == Gate::Legacy || [drive_write::TOOL, calendar_write::TOOL].contains(&name));
    // Every deletion asks in every mode, yolo included: a rule can make it stricter (deny or
    // ask: terminal), never looser. A broken file therefore never lets one run either.
    let gate = match gate {
        Gate::Run | Gate::Legacy if person || every_change || removes => Gate::Client,
        gate => gate,
    };
    Ok((gate, explicit))
}

/// A Drive, Calendar or Photos change that needs a person whatever the rules say
/// (`drive_write::needs_person`, `calendar_write::needs_person`, `photos_write::needs_person`).
/// Arguments that don't validate are left to the tool, which refuses them.
fn drive_needs_person(library: &Library, name: &str, args: &Value) -> Result<bool, &'static str> {
    if name == photos_write::TOOL {
        return Ok(photos_write::validate(args).is_ok_and(|op| photos_write::needs_person(&op)));
    }
    if name == calendar_write::TOOL {
        return Ok(calendar_write::needs_person(args));
    }
    if name != drive_write::TOOL {
        return Ok(false);
    }
    match drive_write::validate(args) {
        Ok(op) => drive_write::needs_person(library, &op),
        Err(_) => Ok(false),
    }
}

/// What an approval is bound to right now: the approval mode (none in a folder without one),
/// the account and the permissions file.
fn current_binding(
    library: &Library,
    configured: Option<&(Policy, ApprovalBinding)>,
    active: &permissions::Active,
) -> Result<ApprovalBinding, &'static str> {
    let mut binding = match configured {
        Some((_, binding)) => binding.clone(),
        None => ApprovalBinding {
            policy: String::new(),
            account: library
                .state(ACCOUNT_KEY)
                .map_err(|_| "policy_unavailable")?,
            identity: (0, 0),
            snapshot: None,
            preview_read: false,
            rules: None,
        },
    };
    binding.rules = Some(active.fingerprint.clone());
    Ok(binding)
}

/// A terminal approval request made because the permissions file says ask: terminal. Bound
/// to the approval mode's revision and the folder, like the request it answers.
fn terminal_proposal(
    library: &mut Library,
    dir: &Path,
    name: &str,
    args: &Value,
    policy: Option<&str>,
    identity: (u64, u64),
    rules: &str,
) -> Result<Value, &'static str> {
    let account = library
        .state(ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .ok_or("login_required")?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut proposal = json!({"tool":name,"args":args,"account":account,
        "binding":{"policy":policy,"state_identity":identity,"rules":rules}});
    // A Photos change carries what it reaches as cached, for the terminal to show and for the
    // approval to hold to; a photo or album not in the cache is refused here, at once.
    let photos = name == photos_write::TOOL;
    if photos {
        proposal["preview"] = photos_write::preview(library, args)?;
    }
    library
        .set_state(&format!("mcp.approval.{id}"), &proposal.to_string())
        .map_err(|_| "library_unavailable")?;
    let mut reply = json!({"status":"approval_required","request_id":id,"approval":"terminal",
        "next":status::with_dir("Ask the person to run skykeep-mcp approve in their own terminal and type this request ID there.", dir),
        "remote_verified":false});
    if photos {
        reply["approve_in_terminal"] = json!(status::with_dir("skykeep-mcp approve", dir));
    }
    Ok(reply)
}

/// The terminal approval `id` names exactly this call under the current mode and folder.
fn terminal_check(
    library: &Library,
    id: &str,
    name: &str,
    args: &Value,
    policy: Option<&str>,
    identity: (u64, u64),
    rules: &str,
) -> Result<(), &'static str> {
    let raw = library
        .state(&format!("mcp.approval.{id}"))
        .map_err(|_| "library_unavailable")?
        .filter(|s| !s.is_empty())
        .ok_or("approval_not_found")?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| "approval_not_found")?;
    let account = library
        .state(ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?;
    if value["tool"] != name
        || value["args"] != *args
        || value["account"] != json!(account)
        || value["binding"] != json!({"policy":policy,"state_identity":identity,"rules":rules})
    {
        return Err("approval_policy_changed");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    mode: String,
    revision: String,
    account: Option<String>,
    state_identity: (u64, u64),
}

/// Library key: the stored approval mode was taken over from the permissions file, not chosen
/// by the person. It counts only while the file names a mode.
const ADOPTED_KEY: &str = "mcp.policy.from_file";
/// Library key: how many waiting changes taking over the file's mode set aside, said once.
const SET_ASIDE_KEY: &str = "mcp.policy.set_aside";

/// Whether the stored approval mode was taken over from the permissions file.
pub(crate) fn adopted(library: &Library) -> bool {
    library.state(ADOPTED_KEY).ok().flatten().as_deref() == Some("1")
}

/// The stored approval mode as the fallback when the file names none: the one setup or
/// `skykeep-mcp policy` stored. One taken over from the file doesn't count then, so the
/// folder is back to its setup choice (no approval mode).
pub(crate) fn stored_mode(library: &Library, dir: &Path) -> Result<Option<String>, &'static str> {
    Ok(policy(library, dir)?
        .map(|(p, _)| p.mode)
        .filter(|_| !adopted(library)))
}

/// The approval mode in force: the permissions file's `approval_mode:`, else the stored one.
/// Read only; the server records file loads at each tool call.
pub(crate) fn mode_in_force(library: &Library, dir: &Path) -> Result<Option<String>, &'static str> {
    let stored = stored_mode(library, dir)?;
    Ok(permissions::peek(library, dir, stored.as_deref())?.mode)
}

fn policy(
    library: &Library,
    dir: &Path,
) -> Result<Option<(Policy, ApprovalBinding)>, &'static str> {
    let Some(raw) = library
        .state("mcp.policy")
        .map_err(|_| "policy_unavailable")?
    else {
        return Ok(None);
    };
    let parsed: Policy = serde_json::from_str(&raw).map_err(|_| "invalid_policy")?;
    if parsed.state_identity != directory_identity(dir)? {
        return Err("policy_state_changed");
    }
    if parsed.account
        != library
            .state(ACCOUNT_KEY)
            .map_err(|_| "policy_unavailable")?
    {
        return Err("policy_account_changed");
    }
    if !["reads_and_writes", "writes_only", "yolo"].contains(&parsed.mode.as_str())
        || parsed.revision.is_empty()
    {
        return Err("invalid_policy");
    }
    Ok(Some((
        parsed,
        ApprovalBinding {
            policy: raw,
            account: library
                .state(ACCOUNT_KEY)
                .map_err(|_| "policy_unavailable")?,
            identity: (0, 0),
            snapshot: None,
            preview_read: false,
            rules: None,
        },
    )))
}

/// An error result. One about the Drive exchange folder says the exact command that sets it
/// up, with this private folder in it, for the person to type in their own terminal.
/// The same for a few Drive outcomes that leave the person something to do, and for a folder
/// inside the exchange folder that others can change (`args` are the call's arguments).
fn error_value(error: &'static str, dir: &Path, args: &Value) -> Value {
    let mut value = plain_error(error);
    if let Some(instructions) = next_step(error, dir, args) {
        value["instructions"] = json!(instructions);
    }
    value
}

/// What the person can do after `error`, when there is something: the sentence an MCP error
/// result carries as `instructions` and the terminal approval prints. `args` are the call's
/// arguments (`Value::Null` when there are none).
pub fn next_step(error: &str, dir: &Path, args: &Value) -> Option<String> {
    if let Some(name) =
        switched_off_service(error).or((error == SYNC_OFF).then_some("Notes or Reminders"))
    {
        let word = name.split(' ').next().unwrap_or(name).to_ascii_lowercase();
        return Some(format!("To switch {name} on, add {word} to the services: line in {} or run {} in your own terminal, then restart the MCP client so it lists the tools again. Changes that were waiting to be sent were set aside; ask for them again. Data cached before it was switched off stays in this folder, and nothing reads it while the service is off; to remove it, set up a fresh folder with skykeep-mcp setup --state-dir NEW_FOLDER and delete this one.", dir.join(permissions::FILE_NAME).display(), status::with_dir("skykeep-mcp setup", dir)));
    }
    if let Some(text) = photos_write::next_step(error, dir, args) {
        return Some(text);
    }
    match error {
        "unsafe_state_contents" => state::unsafe_entry_sentence(dir),
        e if e.starts_with("exchange_folder_") => Some(exchange::next_step(e, dir)),
        // contacts_write's own codes, by their prefix.
        e if e.starts_with("contact_") || e.starts_with("contacts_write_") => {
            contacts_write::next_step(e, args)
        }
        drive_write::UNKNOWN => Some("The connection ended before Apple answered, so the change may or may not have been made. Run drive_refresh on the folder and look at it before trying again.".to_string()),
        drive_write::IN_RECENTLY_DELETED => Some("The item is in Recently Deleted in iCloud Drive but was not deleted for good. Delete it there (iCloud Drive on icloud.com or a Mac), or leave it: Apple deletes it by itself after about 30 days.".to_string()),
        drive_write::IN_RECENTLY_DELETED_LOGIN => Some(format!("The item is in Recently Deleted in iCloud Drive but was not deleted for good, because the sign-in has expired. Sign in again with {} in your own terminal, then delete it from Recently Deleted in iCloud Drive, or leave it: Apple deletes it by itself after about 30 days.", status::with_dir("skykeep-mcp login", dir))),
        "name_taken" => Some("Something with that name is already in that iCloud Drive folder, and it was left as it is. Ask again with another name, or rename or remove the one that is there first.".to_string()),
        "local_file_exists_not_overwritten" => Some("A file with that name is already in the exchange folder, and it was left as it is. Ask again with another local name, or move that file away first.".to_string()),
        "already_has_this_name" => Some("Nothing to do: iCloud Drive already shows the item under that name.".to_string()),
        "already_in_destination" => Some("Nothing to do: the item is already in that folder in iCloud Drive.".to_string()),
        "policy_state_changed" => Some(format!("This folder was copied or moved, so its approval mode belongs to the old folder. In your own terminal, run {} to choose it again.", status::with_dir("skykeep-mcp setup", dir))),
        "policy_account_changed" => Some(format!("The approval mode was set before sign-in or for another account. In your own terminal, run {} to choose it again.", status::with_dir("skykeep-mcp setup", dir))),
        "login_required" => Some(format!("Sign in with {} in a terminal you opened yourself, not one an AI app runs. Never send passwords or codes to MCP or chat.", status::with_dir("skykeep-mcp login", dir))),
        "conflict" => Some("The item changed since you read it, so nothing was queued. Read it again with notes_get or reminders_get, then ask again with its current base_etag.".to_string()),
        "mutation_refused" => Some(format!("The change could not be saved to the local queue, so nothing was queued. In your own terminal, run {} to check this folder, then try again.", status::with_dir("skykeep-mcp status", dir))),
        "drive_read_failed_nothing_sent" => Some("Apple's answer about the item could not be read, so nothing was sent. Try again in a moment; if it keeps failing, run drive_refresh on the folder.".to_string()),
        "local_file_not_found" => Some("No file with that local name is in the exchange folder. Check the name, or put the file there first.".to_string()),
        "local_folder_not_found" => Some("A folder on that local path is not in the exchange folder. Check the path, or make the folder first.".to_string()),
        "local_not_a_folder" => Some("A name on that local path is a file, not a folder. Check the path and ask again.".to_string()),
        "local_symlink_refused" => Some("That local path goes through a symbolic link, which is refused. Use the real file or folder inside the exchange folder.".to_string()),
        "path_not_in_cache_run_drive_refresh" => Some("A folder on that path is not in the local copy of iCloud Drive yet. Run drive_refresh with the same path, then try again.".to_string()),
        "range_too_wide" => Some(format!("The date range is too long. calendar_events and calendar_search take at most {} days, calendar_refresh at most {} days. Ask again with a shorter range.", calendar::MAX_RANGE_DAYS, calendar::MAX_REFRESH_DAYS)),
        photos::APPLE_FAILED => Some("Apple's Photos server was busy, had asked to wait, or sent a reply that could not be used. The local copy was kept as it was. Run photos_refresh again in a few minutes.".to_string()),
        photos::STORE_FAILED => Some(format!("Apple's Photos reply arrived, but it could not be saved to the local copy, which was kept as it was. In your own terminal, run {} to check this folder, then run photos_refresh again.", status::with_dir("skykeep-mcp status", dir))),
        "calendar_service_unavailable" => service_off("Apple Calendar", dir),
        "contacts_service_unavailable" => service_off("Apple Contacts", dir),
        "drive_service_unavailable" => service_off("iCloud Drive", dir),
        "photos_service_unavailable" => service_off("iCloud Photos", dir),
        "local_folder_not_private" => Some(
            args["local"]
                .as_str()
                .and_then(|local| exchange::shared_folder(dir, local))
                .map_or_else(
                    || "A folder inside the exchange folder can be changed by other users. Make it yours alone with chmod 700 on that folder, then try again.".to_string(),
                    |folder| format!("A folder inside the exchange folder can be changed by other users. Make it yours alone in your own terminal with: chmod 700 {} and try again.", status::quoted(&folder)),
                ),
        ),
        other => calendar_write::next_step(other, args),
    }
}

/// The next step when the sign-in does not offer `service`.
fn service_off(service: &str, dir: &Path) -> Option<String> {
    Some(format!("This iCloud sign-in does not offer {service}. Check that {service} is turned on for this Apple Account in iCloud settings, then in your own terminal run {} to sign in again.", status::with_dir("skykeep-mcp setup", dir)))
}

/// [`tool_reply`] with [`error_value`]'s instructions, and the rule that denied the call.
fn tool_reply_in(
    id: Value,
    result: Result<Value, &'static str>,
    dir: &Path,
    args: &Value,
    denied: Option<String>,
) -> Value {
    match result {
        Err(error) => {
            let value = with_rule(error_value(error, dir, args), denied);
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":value.to_string()}],"isError":true}})
        }
        ok => tool_reply(id, ok),
    }
}

/// A deny error names the rule in the permissions file that refused the call.
fn with_rule(mut value: Value, denied: Option<String>) -> Value {
    if let (Some(rule), true) = (denied, value["error"] == DENIED) {
        value["message"] = json!(format!(
            "The rule {rule}: deny in the permissions file refuses this action, so it did not run."
        ));
        value["rule"] = json!(rule);
    }
    value
}

/// The entry in the permissions file that denies this call: the action's own, the tool's, or
/// for a Notes change that deletes, `notes_write.delete`.
fn deny_rule(rules: &permissions::Rules, name: &str, args: &Value) -> String {
    use permissions::Effect;
    let denies = |key: &str| rules.get(key) == Some(&Effect::Deny);
    let own = args["action"].as_str().map(|a| format!("{name}.{a}"));
    match own {
        Some(own) if denies(&own) => own,
        _ if denies(name) => name.to_string(),
        _ if name == "notes_write" && denies("notes_write.delete") => "notes_write.delete".into(),
        _ => name.to_string(),
    }
}

fn tool_reply(id: Value, result: Result<Value, &'static str>) -> Value {
    let failed = result.is_err();
    let value = result.unwrap_or_else(plain_error);
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":value.to_string()}],"isError":failed}})
}

/// The card's first line: what the change does, in one plain sentence, from the same preview
/// the card binds. Names from the cache are shown as escaped JSON text, like the rest of the
/// card, so they can't pose as the card's own words. Empty for a call that changes nothing.
fn card_headline(name: &str, args: &Value, snapshot: Option<&Value>) -> String {
    let Some(action) = args["action"].as_str().filter(|_| name.ends_with("_write")) else {
        return String::new();
    };
    let service = match name {
        "notes_write" => "Notes",
        "reminders_write" => "Reminders",
        contacts_write::TOOL => "Contacts",
        calendar_write::TOOL => "Calendar",
        photos_write::TOOL => "Photos",
        drive_write::TOOL => "iCloud Drive",
        _ => return String::new(),
    };
    let empty = Value::Null;
    let snap = snapshot.unwrap_or(&empty);
    let label = [
        &snap["drive"]["item"]["path"],
        &snap["drive"]["item"]["name"],
        &snap["photos"]["photo"]["title"],
        &snap["photos"]["photo"]["filename"],
        &snap["photos"]["album"]["name"],
        &snap["calendar"]["event"]["title"],
        &snap["contact"]["display_name"],
        &snap["item"]["title"],
        &args["title"],
        &args["name"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().filter(|t| !t.trim().is_empty()))
    .map(|t| {
        let short: String = t.chars().take(120).collect();
        inert_preview(&json!(short)).unwrap_or_default()
    });
    let removal = bound_removal(name, args, snapshot);
    let verb = match (name, action, removal) {
        ("notes_write", "save", permissions::Removal::Permanent) => "Erase all text of".to_string(),
        ("notes_write", "delete", permissions::Removal::Permanent) => "Delete forever".to_string(),
        (_, _, permissions::Removal::Recoverable) => "Move to Recently Deleted".to_string(),
        (_, "delete_forever", _) => "Delete forever".to_string(),
        _ => {
            let words = action.replace('_', " ");
            let mut chars = words.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    };
    // A Calendar delete names its scope: one occurrence or the whole series.
    let scope = if name == calendar_write::TOOL && action == "delete" {
        snap["calendar"]["scope"].as_str()
    } else {
        None
    };
    let series = label.as_deref().unwrap_or("the series");
    let (what, rest) = match (scope, &label) {
        (Some("whole_series"), _) => (
            format!(" every occurrence of {series}, past and future,"),
            "",
        ),
        (Some("this_occurrence"), _) => (
            format!(" one occurrence of {series}"),
            "; the rest of the series stays",
        ),
        (_, Some(l)) => (format!(" {l}"), ""),
        (_, None) => (String::new(), ""),
    };
    let after = match removal {
        permissions::Removal::Permanent => " This can't be undone.",
        permissions::Removal::Recoverable => {
            " Apple keeps it in Recently Deleted for about 30 days."
        }
        permissions::Removal::None => "",
    };
    format!("{verb}{what} in {service}{rest}.{after}")
}

/// What a call removes, judged against the item the card binds. A Notes delete of a note that
/// is already in Recently Deleted is Apple's "Delete Now", which is permanent.
fn bound_removal(name: &str, args: &Value, snapshot: Option<&Value>) -> permissions::Removal {
    let in_trash = snapshot.is_some_and(|snap| {
        snap["item"]["folderGuid"].as_str() == Some(quince_core::notes::TRASH_FOLDER)
    });
    if name == "notes_write" && args["action"] == "delete" && in_trash {
        return permissions::Removal::Permanent;
    }
    permissions::removal(name, args)
}

/// What the person chose on an approval card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Decision {
    Approve,
    AlwaysAllow,
}

/// The card's choices. "Always allow" only where `always` says the card offers it.
fn card_schema(always: bool) -> Value {
    let mut choices = vec![
        json!({"const":"approve","title":"Approve this time"}),
        json!({"const":"deny","title":"Deny"}),
    ];
    if always {
        choices.push(json!({"const":"always_allow","title":"Always allow this action"}));
    }
    json!({"type":"object","properties":{"decision":{"type":"string","title":"Decision","oneOf":choices}},"required":["decision"]})
}

fn decision(response: &Value, always: bool) -> Option<Decision> {
    // The answer to the card is exactly one offered choice. No choice, a declined or
    // cancelled card, or anything else means deny. Result metadata may describe the response
    // but never supplies operation arguments or consent.
    let envelope = response.as_object()?;
    let result = response.get("result").and_then(Value::as_object)?;
    let content = result.get("content").and_then(Value::as_object)?;
    let well_formed = envelope
        .keys()
        .all(|key| ["jsonrpc", "id", "result"].contains(&key.as_str()))
        && response["jsonrpc"] == "2.0"
        && result
            .keys()
            .all(|key| ["action", "content", "_meta"].contains(&key.as_str()))
        && result
            .get("action")
            .is_some_and(|action| action == "accept")
        && content.len() == 1
        && result.get("_meta").is_none_or(Value::is_object);
    if !well_formed {
        return None;
    }
    match content.get("decision").and_then(Value::as_str)? {
        "approve" => Some(Decision::Approve),
        "always_allow" if always => Some(Decision::AlwaysAllow),
        _ => None,
    }
}

fn directory_identity(dir: &Path) -> Result<(u64, u64), &'static str> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir).map_err(|_| "state_unavailable")?;
    Ok((meta.dev(), meta.ino()))
}

struct Limited(Vec<u8>);
impl std::io::Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_PREVIEW_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("preview_too_large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json<T: serde::Serialize>(value: &T) -> Result<Value, &'static str> {
    let mut raw = Limited(Vec::new());
    serde_json::to_writer(&mut raw, value).map_err(|_| "preview_too_large")?;
    let value: Value = serde_json::from_slice(&raw.0).map_err(|_| "invalid_preview")?;
    bounded_value(&value, MAX_ARGUMENT_BYTES)?;
    Ok(value)
}

fn inert_preview(params: &Value) -> Result<String, &'static str> {
    bounded_value(params, MAX_ARGUMENT_BYTES)?;
    let mut raw = Limited(Vec::new());
    serde_json::to_writer_pretty(&mut raw, params).map_err(|_| "preview_too_large")?;
    let raw = String::from_utf8(raw.0).map_err(|_| "invalid_preview")?;
    let mut length = 0usize;
    for c in raw.chars() {
        let size = if !c.is_ascii() || "<>&`*_[]!#~|".contains(c) {
            c.len_utf16() * 6
        } else {
            1
        };
        if size > MAX_PREVIEW_BYTES.saturating_sub(length) {
            return Err("preview_too_large");
        }
        length += size;
    }
    let mut preview = String::with_capacity(length);
    for c in raw.chars() {
        let size = if !c.is_ascii() || "<>&`*_[]!#~|".contains(c) {
            c.len_utf16() * 6
        } else {
            1
        };
        if size > MAX_PREVIEW_BYTES.saturating_sub(preview.len()) {
            return Err("preview_too_large");
        }
        if !c.is_ascii() || "<>&`*_[]!#~|".contains(c) {
            for unit in c.encode_utf16(&mut [0; 2]) {
                preview.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            preview.push(c);
        }
    }
    Ok(preview)
}

pub(crate) fn needs_consent(mode: &str, name: &str) -> Result<bool, &'static str> {
    match name {
        "auth_status" | "sync_status" => Ok(false),
        "notes_write" | "reminders_write" => Ok(mode != "yolo"),
        "notes_get" | "notes_list" | "notes_search" | "notes_folders" | "reminders_get"
        | "reminders_list" | "reminders_search" | "reminders_lists" => {
            Ok(mode == "reads_and_writes")
        }
        "sync" => Ok(mode == "reads_and_writes"),
        // Contacts content and its refresh are reads: prompted only under reads_and_writes.
        "contacts_list" | "contacts_search" | "contacts_get" | "contacts_refresh" => {
            Ok(mode == "reads_and_writes")
        }
        // Calendar content and its refresh are reads too.
        "calendar_calendars" | "calendar_events" | "calendar_search" | "calendar_get"
        | "calendar_refresh" => Ok(mode == "reads_and_writes"),
        // Photos metadata and its refresh are reads too.
        "photos_albums" | "photos_list" | "photos_search" | "photos_get" | "photos_refresh" => {
            Ok(mode == "reads_and_writes")
        }
        // Drive metadata and its refresh are reads too.
        "drive_list" | "drive_get" | "drive_search" | "drive_refresh" => {
            Ok(mode == "reads_and_writes")
        }
        // Every Drive, Photos, Calendar and Contacts change is asked on the card, except under
        // yolo. Deletions and large Drive folder moves still ask there then (gate_for).
        "drive_write" | "photos_write" | "calendar_write" | "contacts_write" => Ok(mode != "yolo"),
        _ => Err("unknown_tool"),
    }
}

fn resolved_snapshot(
    library: &Library,
    name: &str,
    args: &Value,
    active: &permissions::Active,
) -> Result<Value, &'static str> {
    if name == drive_write::TOOL {
        // Drive has no queue: the preview of what the action reaches is the whole context.
        let snapshot = json!({"drive": drive_write::preview(library, args)?});
        bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
        return Ok(snapshot);
    }
    if name == photos_write::TOOL {
        let snapshot = json!({"photos": photos_write::preview(library, args)?});
        bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
        return Ok(snapshot);
    }
    if name == calendar_write::TOOL {
        let snapshot = json!({"calendar": calendar_write::preview(
            library,
            args,
            permissions::invitations_allowed(active),
            false,
        )?});
        bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
        return Ok(snapshot);
    }
    if name == contacts_write::TOOL {
        // The preview names the card itself; no wrapper around it.
        let snapshot = contacts_write::preview(library, args)?;
        bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
        return Ok(snapshot);
    }
    let guid = args["guid"].as_str().ok_or("guid_required")?;
    if name == "notes_write" {
        let _: NoteWrite = serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    } else if name == "reminders_write" {
        let _: ReminderWrite =
            serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    }
    let mut snapshot = resolved_state(library, name, guid)?;
    let queued: Vec<_> = sync::pending_operations(library)?
        .into_iter()
        .filter(|p| p.record == guid)
        .map(|p| json!({"id":p.id,"kind":p.kind,"payload":p.payload}))
        .collect();
    snapshot["queued_operations"] = json!(queued);
    bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
    Ok(snapshot)
}

fn resolved_state(library: &Library, name: &str, guid: &str) -> Result<Value, &'static str> {
    let snapshot = if name == "notes_write" {
        let mut item = library
            .note(guid)
            .map_err(|_| "library_unavailable")?
            .ok_or("not_found")?;
        item.body = library.note_body(guid).map_err(|_| "library_unavailable")?;
        // Note.runs intentionally does not serialize. Bind its complete cached
        // representation explicitly, with the same budgets as the rest of the preview.
        let runs = bounded_json(&library.note_runs(guid).map_err(|_| "library_unavailable")?)?;
        let runs = match runs {
            Value::String(raw) => {
                serde_json::from_str::<Value>(&raw).map_err(|_| "invalid_cached_runs")?
            }
            _ => Value::Null,
        };
        bounded_value(&runs, MAX_ARGUMENT_BYTES)?;
        let item = bounded_json(&item)?;
        let containers = bounded_json(&library.note_folders().map_err(|_| "library_unavailable")?)?;
        json!({"item":item,"runs":runs,"containers":containers})
    } else if name == "reminders_write" {
        let item = bounded_json(
            &library
                .reminder(guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?,
        )?;
        let detail = bounded_json(
            &library
                .reminder_detail(guid)
                .map_err(|_| "library_unavailable")?,
        )?;
        let children = bounded_json(
            &library
                .subtasks_of(guid)
                .map_err(|_| "library_unavailable")?,
        )?;
        let containers = bounded_json(
            &library
                .reminder_lists()
                .map_err(|_| "library_unavailable")?,
        )?;
        json!({"item":item,"detail":detail,"children":children,"containers":containers})
    } else {
        return Err("unknown_tool");
    };
    bounded_value(&snapshot, MAX_ARGUMENT_BYTES)?;
    Ok(snapshot)
}

pub struct Server {
    /// `None` until the first operation that holds the private directory's lock.
    library: Option<Library>,
    dir: std::path::PathBuf,
    initialized: bool,
    forms: bool,
    pending: Option<PendingConsent>,
    grant: Option<ApprovalBinding>,
    /// The terminal approval request `skykeep-mcp approve` is running, if any.
    terminal: Option<String>,
    /// The rule that denied the last call, for its error message.
    denied: Option<String>,
    /// Set by the last consent check: the card must not offer "Always allow", because an
    /// allow rule would not stop it asking (a deletion or a large Drive folder move).
    always_blocked: bool,
    drive: drive_write::Seam,
    calendar: calendar_write::Seam,
    contacts: contacts_write::Seam,
    /// How long a card waits for its answer.
    card_timeout: std::time::Duration,
}

#[cfg(test)]
mod consent_tests {
    use super::*;

    /// A queued write is recorded under the mode the gate decided with, not one read again
    /// afterwards: a file made stricter in between leaves it marked looser, so it is set
    /// aside instead of sent.
    #[test]
    fn a_queued_write_keeps_the_mode_its_gate_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let mut server = Server::open(&dir).unwrap();
        let mut library = Library::open(&dir).unwrap();
        library.set_state(ACCOUNT_KEY, "synthetic").unwrap();
        server.set_policy("yolo").unwrap();
        // The file turned stricter after the gate let the write through under yolo.
        let file = dir.join(permissions::FILE_NAME);
        std::fs::write(&file, "version: 1\napproval_mode: writes_only\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let args: NoteWrite =
            serde_json::from_value(json!({"action":"create","body":"synthetic"})).unwrap();
        authorized_enqueue(&mut library, &dir, Some("yolo"), |l| enqueue_note(l, args)).unwrap();
        let pending = library.outbox_all_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            sync::authorized(&library, &dir, &pending[0]),
            Err("write_mode_stricter")
        );
    }
    #[tokio::test]
    async fn timeout_and_disconnect_drop_unconsumed_consent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut server = Server::open(dir.path()).unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        library.set_state(ACCOUNT_KEY, "synthetic").unwrap();
        server.set_policy("writes_only").unwrap();
        server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
        let request = json!({"id":2,"method":"tools/call","params":{"name":"notes_write","arguments":{"action":"create","body":"exact"}}});
        let form = server.handle(request.clone()).await.unwrap();
        server.pending.as_mut().unwrap().deadline = std::time::Instant::now();
        assert_eq!(server.expire_consent().unwrap()["id"], 2);
        assert!(server
            .handle(
                json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}})
            )
            .await
            .is_none());
        server.handle(request).await;
        drop(server); // EOF drops the only pending record; nothing is persisted.
        assert_eq!(library.outbox_count().unwrap(), 0);
        let mut reopened = Server::open(dir.path()).unwrap();
        assert!(reopened
            .handle(
                json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}})
            )
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_timed_out_contacts_read_returns_nothing_and_its_late_answer_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut server = Server::open(dir.path()).unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        library.set_state(ACCOUNT_KEY, "synthetic").unwrap();
        let mut card = quince_core::contacts::Contact {
            guid: "C-1".into(),
            first_name: Some("Synthetic".into()),
            middle_name: None,
            photo_url: None,
            photo_signature: None,
            photo_crop: None,
            last_name: Some("Person".into()),
            company: None,
            is_company: false,
            birthday: None,
            notes: None,
            etag: None,
            methods: Vec::new(),
        };
        card.notes = Some("SYNTHETIC-PRIVATE".into());
        library.put_contacts(&[card]).unwrap();
        server.set_policy("reads_and_writes").unwrap();
        server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}}}})).await;
        for (name, args) in [
            ("contacts_get", json!({"guid":"C-1"})),
            ("contacts_refresh", json!({})),
        ] {
            let form = server
                .handle(
                    json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":args}}),
                )
                .await
                .unwrap();
            assert_eq!(form["method"], "elicitation/create");
            server.pending.as_mut().unwrap().deadline = std::time::Instant::now();
            let expired = server.expire_consent().unwrap();
            assert_eq!(expired["id"], 2);
            assert!(expired.to_string().contains("approval_timeout"));
            assert!(!expired.to_string().contains("SYNTHETIC-PRIVATE"));
            assert!(server
                .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
                .await
                .is_none());
        }
        assert!(library
            .sync_health()
            .unwrap()
            .iter()
            .all(|h| h.service != "contacts" || h.last_error.is_none()));
    }
}

impl Server {
    fn prepare_consent(
        &mut self,
        params: &Value,
        read_grant: Option<&ApprovalBinding>,
    ) -> Result<Option<ApprovalBinding>, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        let dir = held.path.clone();
        let library = self.ready(&held)?;
        let configured = policy(library, &held.path)?;
        let stored = stored_mode(library, &held.path)?;
        let active = permissions::load(library, &dir, stored.as_deref())?;
        let configured = match configured {
            None if adopt_file_mode(library, &dir, &active)? => policy(library, &dir)?,
            configured => configured,
        };
        // The file's approval_mode, when it names one, replaces the mode setup stored.
        let mode = active.mode.clone();
        let name = params["name"].as_str().ok_or("invalid_tool")?;
        let (gate, _) = gate_for(
            library,
            mode.as_deref(),
            &active,
            name,
            &params["arguments"],
        )?;
        if gate == Gate::Deny {
            self.denied = Some(deny_rule(&active.rules, name, &params["arguments"]));
            return Err(DENIED);
        }
        // Malformed Contacts arguments are refused before anyone is asked to approve them.
        contacts::validate(name, &params["arguments"])?;
        calendar::validate(name, &params["arguments"])?;
        photos::validate(name, &params["arguments"])?;
        drive::validate(name, &params["arguments"])?;
        if name == photos_write::TOOL {
            photos_write::validate(&params["arguments"])?;
        }
        if gate != Gate::Client {
            return Ok(None);
        }
        // Whether an allow rule for this action would still leave it asking.
        let blocked = match permissions::name_for(name, &params["arguments"]) {
            Some(rule) => {
                let mut rules = active.rules.clone();
                rules.insert(rule, permissions::Effect::Allow);
                let allowed = permissions::Active {
                    rules,
                    problem: None,
                    kept_from: None,
                    ..active.clone()
                };
                gate_for(
                    library,
                    mode.as_deref(),
                    &allowed,
                    name,
                    &params["arguments"],
                )?
                .0 != Gate::Run
            }
            None => true,
        };
        let mut binding = current_binding(library, configured.as_ref(), &active)?;
        if name.ends_with("_write") && binding.account.is_none() {
            return Err("login_required");
        }
        binding.identity = directory_identity(&dir)?;
        if name.ends_with("_write") && params["arguments"]["action"] != "create" {
            if mode.as_deref() == Some("reads_and_writes") {
                if let Some(previous) = read_grant {
                    let mut expected = binding.clone();
                    expected.preview_read = true;
                    if previous != &expected {
                        return Err("approval_policy_changed");
                    }
                } else {
                    binding.preview_read = true;
                    return Ok(Some(binding));
                }
            }
            binding.snapshot = Some(resolved_snapshot(
                library,
                name,
                &params["arguments"],
                &active,
            )?);
        } else if name == calendar_write::TOOL {
            // A new event has no cached item, but its preview says what the card approves.
            binding.snapshot = Some(resolved_snapshot(
                library,
                name,
                &params["arguments"],
                &active,
            )?);
        }
        self.always_blocked = blocked;
        Ok(Some(binding))
    }

    fn consent_form(&mut self, mut request: Value, binding: ApprovalBinding) -> Value {
        // A default Calendar refresh window depends on the day: fixed now, so the approved
        // dates are the ones that run.
        if request["params"]["name"] == "calendar_refresh" {
            match calendar::resolved_refresh_args(&request["params"]["arguments"]) {
                Ok(args) => request["params"]["arguments"] = args,
                Err(error) => return tool_reply(request["id"].clone(), Err(error)),
            }
        }
        let refresh = match request["params"]["name"].as_str() {
            Some("sync") => Some(
                json!({"services":["notes","reminders"],"refresh":"one bounded pass, at most 60 seconds","send":"only unchanged previously authorized durable operations, at most 128"}),
            ),
            Some("contacts_refresh") => Some(
                json!({"services":["contacts"],"refresh":"one read of Apple iCloud Contacts into the local cache, at most 60 seconds","send":"nothing is sent; queued changes stay queued"}),
            ),
            Some("calendar_refresh") => {
                Some(calendar::refresh_scope(&request["params"]["arguments"]))
            }
            Some("photos_refresh") => Some(photos::refresh_scope()),
            Some("drive_refresh") => Some(drive::refresh_scope()),
            _ => None,
        };
        let name = request["params"]["name"].as_str().unwrap_or_default();
        let arguments = &request["params"]["arguments"];
        // "Always allow" saves a rule, so it is never offered for a preview read, for any
        // deletion, trash or move to Recently Deleted, or where an allow rule would still ask
        // (prepare_consent).
        let removal = bound_removal(name, arguments, binding.snapshot.as_ref());
        let always = (!binding.preview_read
            && removal == permissions::Removal::None
            && !self.always_blocked)
            .then(|| permissions::name_for(name, arguments))
            .flatten();
        let headline = if binding.preview_read {
            "Approving lets the assistant read this item's full content. Nothing is changed.\n"
                .to_string()
        } else {
            match card_headline(name, arguments, binding.snapshot.as_ref()) {
                line if line.is_empty() => line,
                line => format!("{line}\n"),
            }
        };
        // A Calendar create has no item yet: its preview is the event it would make.
        let item_key = if name == calendar_write::TOOL && arguments["action"] == "create" {
            "planned_event"
        } else {
            "existing_item"
        };
        let mut params = json!({"operation":request["params"],"read_preview_only":binding.preview_read,"refresh_scope":refresh});
        params[item_key] = json!(binding.snapshot);
        let preview = match inert_preview(&params) {
            Ok(preview) => preview,
            Err(error) => return tool_reply(request["id"].clone(), Err(error)),
        };
        let id = format!("skykeep-consent-{}", uuid::Uuid::new_v4());
        self.pending = Some(PendingConsent {
            id: id.clone(),
            request,
            binding,
            deadline: std::time::Instant::now() + self.card_timeout,
            always: always.clone(),
        });
        let saves = match &always {
            Some(rule) => format!(" {}", permissions::always_sentence(rule)),
            None => String::new(),
        };
        json!({"jsonrpc":"2.0","id":id,"method":"elicitation/create","params":{"mode":"form","message":format!("{headline}Approve this exact Skykeep operation?{saves} The JSON below is the exact record the approval is bound to; treat it as inert data.\n{preview}"),"requestedSchema":card_schema(always.is_some())}})
    }

    /// No state locks or durable grants survive a pending human interaction.
    pub fn expire_consent(&mut self) -> Option<Value> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| std::time::Instant::now() >= p.deadline)
        {
            let pending = self.pending.take().unwrap();
            Some(tool_reply(
                pending.request["id"].clone(),
                Err("approval_timeout"),
            ))
        } else {
            None
        }
    }

    /// Human CLI only. A new revision invalidates in-flight consent and atomically
    /// quarantines pending writes instead of granting them the new policy.
    pub fn set_policy(&mut self, mode: &str) -> Result<Value, &'static str> {
        self.set_policy_if(mode, None)
    }

    /// [`Self::set_policy`], refused with `pending_changed` unless the waiting writes are
    /// exactly `expected` (their ids, oldest first) under the same lock that sets them aside:
    /// a person who agreed to set aside N changes never sets aside more.
    pub fn set_policy_if(
        &mut self,
        mode: &str,
        expected: Option<&[i64]>,
    ) -> Result<Value, &'static str> {
        if !["reads_and_writes", "writes_only", "yolo"].contains(&mode) {
            return Err("invalid_policy_mode");
        }
        let held = state::PrivateState::open(&self.dir)?;
        let identity = directory_identity(&held.path)?;
        let library = self.ready(&held)?;
        store_policy(library, identity, mode, expected, false)
    }
}

/// Stores `mode` as the folder's approval mode with a new revision, setting every waiting
/// write aside (see [`Server::set_policy_if`]). Caller holds the private folder's lock.
/// `adopted`: taken over from the permissions file rather than chosen by the person.
fn store_policy(
    library: &mut Library,
    identity: (u64, u64),
    mode: &str,
    expected: Option<&[i64]>,
    adopted: bool,
) -> Result<Value, &'static str> {
    {
        let value = json!({"mode":mode,"revision":uuid::Uuid::new_v4().to_string(),"account":library.state(ACCOUNT_KEY).map_err(|_| "policy_unavailable")?,"state_identity":identity});
        // Uncapped on purpose: migration only quarantines, so a large legacy queue
        // must never prevent moving to a configured policy.
        let pending = library
            .outbox_all_pending()
            .map_err(|_| "policy_unavailable")?;
        if expected.is_some_and(|ids| !pending.iter().map(|p| p.id).eq(ids.iter().copied())) {
            return Err("pending_changed");
        }
        library
            .atomically(|library| -> quince_core::Result<()> {
                for p in &pending {
                    library.outbox_refuse(p.id, "mcp_policy_migration_quarantine")?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    if ["notes", "reminders"].contains(&p.service.as_str()) {
                        library.set_state(&sync::reread_key(&p.service), "1")?;
                    }
                }
                library.set_state(ADOPTED_KEY, if adopted { "1" } else { "" })?;
                let set_aside = if adopted && !pending.is_empty() {
                    pending.len().to_string()
                } else {
                    String::new()
                };
                library.set_state(SET_ASIDE_KEY, &set_aside)?;
                library.set_state("mcp.policy", &value.to_string())
            })
            .map_err(|_| "policy_unavailable")?;
        let saved = library
            .state("mcp.policy")
            .map_err(|_| "policy_unavailable")?
            .ok_or("policy_unavailable")?;
        serde_json::from_str(&saved).map_err(|_| "policy_unavailable")
    }
}

/// A folder without a stored approval mode whose permissions file names one takes that mode
/// as its stored one, as `skykeep-mcp policy` would: queued writes then carry a grant bound
/// to it, and a later stricter mode sets them aside. Writes already waiting are set aside, as
/// for any first mode. Nothing happens before sign-in, where a mode would be bound to no
/// account. Returns whether a mode was stored.
fn adopt_file_mode(
    library: &mut Library,
    dir: &Path,
    active: &permissions::Active,
) -> Result<bool, &'static str> {
    let Some(mode) = active.mode.as_deref().filter(|_| active.mode_in_file) else {
        return Ok(false);
    };
    if library
        .state("mcp.policy")
        .map_err(|_| "policy_unavailable")?
        .is_some()
        || library
            .state(ACCOUNT_KEY)
            .map_err(|_| "library_unavailable")?
            .is_none()
    {
        return Ok(false);
    }
    store_policy(library, directory_identity(dir)?, mode, None, true)?;
    Ok(true)
}

impl Server {
    /// Human CLI only. Refused writes kept for review, without their payloads.
    pub fn quarantined(&mut self) -> Result<Vec<Value>, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        let library = self.ready(&held)?;
        let refused = library
            .outbox_refused()
            .map_err(|_| "library_unavailable")?;
        // Upstream reasons can hold HTTP text; the terminal gets a bounded line with no
        // control or text-direction characters that could rearrange what is shown.
        let plain = |text: &str, limit: usize| -> String {
            text.chars()
                .filter(|c| {
                    !c.is_control()
                        && !matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
                })
                .take(limit)
                .collect()
        };
        Ok(refused
            .iter()
            .map(|r| {
                json!({"id":r.change.id,"service":plain(&r.change.service, 40),"kind":plain(&r.change.kind, 40),
                    "record":plain(&r.change.record, 200),"reason":plain(&r.reason, 160),"refused_at":r.refused_at})
            })
            .collect())
    }

    /// Human CLI only, after `quarantined` was shown. Discards exactly these ids while they
    /// are still refused; waiting writes and rows refused later are left alone. Retired
    /// writes are never sent.
    pub fn retire_quarantined(&mut self, ids: &[i64]) -> Result<usize, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        let library = self.ready(&held)?;
        let refused = library
            .outbox_refused()
            .map_err(|_| "library_unavailable")?;
        let chosen: Vec<&quince_core::outbox::Pending> = refused
            .iter()
            .map(|r| &r.change)
            .filter(|p| ids.contains(&p.id))
            .collect();
        library
            .atomically(|library| -> quince_core::Result<()> {
                for p in &chosen {
                    library.outbox_discard(p.id)?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    quince_sync::reminders::settle(library, p)?;
                }
                Ok(())
            })
            .map_err(|_| "library_unavailable")?;
        Ok(chosen.len())
    }

    /// Starts even while another process holds the directory lock, for example during its
    /// sync. Setup is then deferred, and each call reports `state_busy_retry` until the lock
    /// is free.
    pub fn open(dir: &Path) -> Result<Self, &'static str> {
        match state::PrivateState::open(dir) {
            Ok(held) => Ok(Self {
                library: Some(setup(&held.path)?),
                dir: held.path.clone(),
                initialized: false,
                forms: false,
                pending: None,
                grant: None,
                terminal: None,
                always_blocked: false,
                denied: None,
                drive: drive_write::Seam::default(),
                calendar: calendar_write::Seam::default(),
                contacts: contacts_write::Seam::default(),
                card_timeout: CARD_TIMEOUT,
            }),
            Err("state_busy_retry") => Ok(Self {
                library: None,
                dir: std::path::absolute(dir).map_err(|_| "state_unavailable")?,
                initialized: false,
                forms: false,
                pending: None,
                grant: None,
                terminal: None,
                always_blocked: false,
                denied: None,
                drive: drive_write::Seam::default(),
                calendar: calendar_write::Seam::default(),
                contacts: contacts_write::Seam::default(),
                card_timeout: CARD_TIMEOUT,
            }),
            Err(error) => Err(error),
        }
    }

    /// Caller holds the directory lock through `held`.
    fn ready(&mut self, held: &state::PrivateState) -> Result<&mut Library, &'static str> {
        if self.library.is_none() {
            self.library = Some(setup(&held.path)?);
            self.dir = held.path.clone();
        }
        self.library.as_mut().ok_or("state_unavailable")
    }

    /// Test seam: Drive changes against synthetic endpoints and limits. Never set in
    /// production, and no MCP method reaches it.
    #[doc(hidden)]
    pub fn set_drive_seam(&mut self, seam: drive_write::Seam) {
        self.drive = seam;
    }

    /// Test seam: Calendar changes against synthetic endpoints and limits, as for Drive.
    #[doc(hidden)]
    pub fn set_calendar_seam(&mut self, seam: calendar_write::Seam) {
        self.calendar = seam;
    }

    #[doc(hidden)]
    pub fn set_contacts_seam(&mut self, seam: contacts_write::Seam) {
        self.contacts = seam;
    }

    /// Test seam: how long the next card waits, as for the other seams.
    #[doc(hidden)]
    pub fn set_card_timeout(&mut self, timeout: std::time::Duration) {
        self.card_timeout = timeout;
    }

    /// Trusted local CLI only, after `approval_details` was shown: runs one approved
    /// proposal. A Drive change runs now, against Apple; a Notes or Reminders change is queued.
    pub async fn approve_any(&mut self, id: &str) -> Result<Value, &'static str> {
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        let tool = {
            let held = state::PrivateState::open(&self.dir)?;
            let library = self.ready(&held)?;
            let raw = library
                .state(&format!("mcp.approval.{id}"))
                .map_err(|_| "library_unavailable")?
                .filter(|s| !s.is_empty())
                .ok_or("approval_not_found")?;
            serde_json::from_str::<Value>(&raw).map_err(|_| "approval_not_found")?["tool"].clone()
        };
        if tool == drive_write::TOOL || tool == calendar_write::TOOL {
            self.approve_drive(id).await
        } else {
            self.approve_request(id).await
        }
    }

    /// A Drive or Calendar proposal holds to the account, the approval mode (revision
    /// included), the private directory and the cached preview it was made with: any of them
    /// changed, it is refused and must be asked again. It is used up before it runs, so it runs
    /// once.
    async fn approve_drive(&mut self, id: &str) -> Result<Value, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        let dir = held.path.clone();
        let seam = self.drive.clone();
        let calendar_seam = self.calendar.clone();
        let library = self.ready(&held)?;
        let key = format!("mcp.approval.{id}");
        let raw = library
            .state(&key)
            .map_err(|_| "library_unavailable")?
            .filter(|s| !s.is_empty())
            .ok_or("approval_not_found")?;
        let proposal: Value = serde_json::from_str(&raw).map_err(|_| "approval_not_found")?;
        let tool = proposal["tool"].as_str().unwrap_or_default().to_string();
        if proposal["account"]
            != json!(library
                .state(ACCOUNT_KEY)
                .map_err(|_| "library_unavailable")?)
        {
            return Err("account_mismatch");
        }
        policy(library, &dir)?;
        if proposal["policy"]
            != json!(library
                .state("mcp.policy")
                .map_err(|_| "policy_unavailable")?)
            || proposal["state_identity"] != json!(directory_identity(&dir)?)
        {
            return Err("approval_policy_changed");
        }
        // The permissions file as it is now: a later deny refuses, and any edit since the
        // request makes it stale, as for other terminal requests.
        let stored = stored_mode(library, &dir)?;
        let active = permissions::load(library, &dir, stored.as_deref())?;
        let mode = active.mode.clone();
        if gate_for(library, mode.as_deref(), &active, &tool, &proposal["args"])?.0 == Gate::Deny {
            return Err(DENIED);
        }
        if proposal["rules"] != json!(active.fingerprint) {
            return Err("approval_policy_changed");
        }
        if tool == calendar_write::TOOL {
            let invitations = permissions::invitations_allowed(&active);
            if calendar_write::preview(library, &proposal["args"], invitations, true)?
                != proposal["preview"]
            {
                return Err("approval_stale_calendar_changed_ask_again");
            }
            let op = calendar_write::validate(&proposal["args"])?;
            library
                .set_state(&key, "")
                .map_err(|_| "library_unavailable")?;
            return calendar_write::run(&dir, &calendar_seam, op, true, invitations).await;
        }
        if drive_write::preview(library, &proposal["args"])? != proposal["preview"] {
            return Err("approval_stale_drive_changed_ask_again");
        }
        let op = drive_write::validate(&proposal["args"])?;
        library
            .set_state(&key, "")
            .map_err(|_| "library_unavailable")?;
        drive_write::run(&dir, &seam, op, true).await
    }

    /// The services on now, read without recording anything, for the tool list and the
    /// server instructions. While another process holds the folder, the file alone decides
    /// when it can be used. `None` when nothing can say: then no service is called off here,
    /// and every call is still checked against the file when it runs.
    fn services_now(&mut self) -> Option<Vec<&'static str>> {
        match state::PrivateState::open(&self.dir) {
            Ok(held) => {
                let library = self.ready(&held).ok()?;
                let stored = stored_mode(library, &held.path).ok().flatten();
                permissions::peek(library, &held.path, stored.as_deref())
                    .ok()
                    .map(|a| a.services)
            }
            Err(_) => permissions::services_in_file(&self.dir),
        }
    }

    pub async fn refresh(&mut self) -> Result<Value, &'static str> {
        self.call("sync", json!({})).await
    }

    pub async fn handle(&mut self, request: Value) -> Option<Value> {
        if bounded_value(&request, MAX_FRAME_BYTES).is_err() {
            return Some(tool_reply(Value::Null, Err("operation_too_large")));
        }
        if request["method"] == "notifications/cancelled" {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.request["id"] == request["params"]["requestId"])
            {
                let pending = self.pending.take().unwrap();
                return Some(tool_reply(
                    pending.request["id"].clone(),
                    Err("approval_cancelled"),
                ));
            }
            return None;
        }
        if request.get("method").is_none()
            && (request.get("result").is_some() || request.get("error").is_some())
        {
            if !self.pending.as_ref().is_some_and(|p| request["id"] == p.id) {
                return None;
            }
            let pending = self.pending.take().unwrap();
            let chosen = decision(&request, pending.always.is_some())
                .filter(|_| std::time::Instant::now() < pending.deadline);
            let Some(chosen) = chosen else {
                return Some(tool_reply(
                    pending.request["id"].clone(),
                    Err("approval_denied"),
                ));
            };
            if pending.binding.preview_read {
                return Some(
                    match self.prepare_consent(&pending.request["params"], Some(&pending.binding)) {
                        Ok(Some(binding)) => self.consent_form(pending.request, binding),
                        _ => tool_reply(
                            pending.request["id"].clone(),
                            Err("approval_policy_changed"),
                        ),
                    },
                );
            }
            let fingerprint = pending.binding.rules.clone().unwrap_or_default();
            self.grant = Some(pending.binding);
            let params = &pending.request["params"];
            let result = self
                .call(
                    params["name"].as_str().unwrap_or(""),
                    params["arguments"].clone(),
                )
                .await;
            self.grant = None;
            let result = match (chosen, pending.always, result) {
                (Decision::AlwaysAllow, Some(rule), Ok(mut value)) => {
                    // The person chose this on the card; the agent can't answer the card.
                    let saved = state::PrivateState::open(&self.dir)
                        .map_err(|e| e.to_string())
                        .and_then(|held| permissions::save_allow(&held.path, &rule, &fingerprint));
                    if value.is_object() {
                        value["permission_rule"] = match saved {
                            Ok(()) => json!({"saved":true,"rule":format!("{rule}: allow")}),
                            Err(reason) => {
                                json!({"saved":false,"rule":format!("{rule}: allow"),"reason":format!("The action ran once, but the rule was not saved: {reason}.")})
                            }
                        };
                    }
                    Ok(value)
                }
                (_, _, result) => result,
            };
            return Some(tool_reply_in(
                pending.request["id"].clone(),
                result,
                &self.dir,
                &pending.request["params"]["arguments"],
                self.denied.take(),
            ));
        }
        // Batches and other non-object messages are not supported; say so instead of dropping them.
        if !request.is_object() {
            return Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request: batches are not supported"}}),
            );
        }
        // Responses belong to server-originated RPCs, never to the tool dispatcher.
        // Pure responses were handled above. Discard ambiguous envelopes carrying
        // both a method and a result/error instead of dispatching their method.
        if request.get("result").is_some() || request.get("error").is_some() {
            return None;
        }
        let id = request.get("id")?.clone();
        if !id.is_string() && !id.is_i64() && !id.is_u64() {
            return Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request ID"}}),
            );
        }
        let result = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                if self.initialized {
                    return Some(tool_reply(id, Err("already_initialized")));
                }
                self.initialized = true;
                let version = request["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or("2025-03-26");
                let elicitation = &request["params"]["capabilities"]["elicitation"];
                self.forms = version == "2025-11-25"
                    && elicitation.as_object().is_some_and(|c| {
                        c.is_empty() || c.get("form").is_some_and(Value::is_object)
                    });
                let version = if version == "2025-11-25" {
                    version
                } else {
                    "2025-03-26"
                };
                let mut server_info =
                    json!({"name":"skykeep-mcp","version":env!("CARGO_PKG_VERSION")});
                // Implementation.title exists only from protocol 2025-06-18 on.
                if version == "2025-11-25" {
                    server_info["title"] = json!(SERVER_TITLE);
                }
                let mut instructions = SERVER_INSTRUCTIONS.to_string();
                let off: Vec<&str> = match self.services_now() {
                    Some(on) => permissions::SERVICES
                        .iter()
                        .map(|(s, _)| *s)
                        .filter(|s| !on.contains(s))
                        .collect(),
                    None => Vec::new(),
                };
                if !off.is_empty() {
                    instructions.push_str(&format!(" Switched off in this folder: {}. Their tools are not offered and are refused.", permissions::service_names(&off)));
                }
                Ok(
                    json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":server_info,"instructions":instructions}),
                )
            }
            Some("ping") => Ok(json!({})),
            Some("tools/list") if self.initialized => {
                let on = self.services_now();
                Ok(json!({"tools":listed_tools(on.as_deref())}))
            }
            Some("tools/call") if self.initialized => {
                if bounded_value(&request["params"], MAX_ARGUMENT_BYTES).is_err() {
                    return Some(tool_reply(id, Err("operation_too_large")));
                }
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                if self.pending.is_some()
                    && !["auth_status", "sync_status"]
                        .contains(&params["name"].as_str().unwrap_or(""))
                {
                    return Some(tool_reply(id, Err("approval_pending")));
                }
                match self.prepare_consent(&params, None) {
                    Ok(Some(binding)) => {
                        if !self.forms {
                            return Some(tool_reply(id, Err("approval_requires_form_client")));
                        }
                        return Some(self.consent_form(request, binding));
                    }
                    Err(error) => {
                        let denied = self.denied.take();
                        return Some(tool_reply_in(
                            id,
                            Err(error),
                            &self.dir,
                            &params["arguments"],
                            denied,
                        ));
                    }
                    Ok(None) => {}
                }
                let result = self
                    .call(
                        params["name"].as_str().unwrap_or(""),
                        params.get("arguments").cloned().unwrap_or(json!({})),
                    )
                    .await;
                let (value, failed) = match result {
                    Ok(value) => (value, false),
                    Err(error) => (
                        with_rule(
                            error_value(error, &self.dir, &params["arguments"]),
                            self.denied.take(),
                        ),
                        true,
                    ),
                };
                Ok(json!({"content":[{"type":"text","text":value.to_string()}],"isError":failed}))
            }
            _ => Err((-32601, "Method not found")),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        })
    }

    /// Trusted local CLI only. No MCP method calls this function.
    pub fn approval_details(&self, id: &str) -> Result<String, &'static str> {
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        self.library
            .as_ref()
            .ok_or("state_busy_retry")?
            .state(&format!("mcp.approval.{id}"))
            .map_err(|_| "library_unavailable")?
            .filter(|s| !s.is_empty())
            .ok_or("approval_not_found")
    }

    /// Trusted local CLI only: what `approve` shows before it asks. A Drive change is said in
    /// plain lines (what it does, the item and its path, what it reaches), a Contacts change
    /// too (the card, and each field with its current and new value); a Notes or Reminders
    /// change as its JSON. Control and text-direction characters are shown as
    /// U+FFFD, so nothing in the text can reorder or hide what the terminal shows.
    pub fn approval_text(&self, id: &str) -> Result<String, &'static str> {
        let raw = self.approval_details(id)?;
        let proposal: Value = serde_json::from_str(&raw).map_err(|_| "approval_not_found")?;
        let text = if proposal["tool"] == drive_write::TOOL {
            drive_write::plain_preview(&proposal["preview"])
        } else if proposal["tool"] == photos_write::TOOL {
            photos_write::plain_preview(&proposal["preview"])
        } else if proposal["tool"] == calendar_write::TOOL {
            calendar_write::plain_preview(&proposal["preview"])
        } else if proposal["tool"] == contacts_write::TOOL {
            let library = self.library.as_ref().ok_or("state_busy_retry")?;
            contacts_write::approval_text(library, &proposal["args"])?
        } else {
            raw
        };
        Ok(text
            .chars()
            .map(|c| {
                if (c.is_control() && c != '\n') || exchange::hides_text(c) {
                    '\u{FFFD}'
                } else {
                    c
                }
            })
            .collect())
    }

    pub fn approve(&mut self, id: &str) -> Result<Value, &'static str> {
        let held = state::PrivateState::open(&self.dir)?;
        if policy(self.ready(&held)?, &held.path)?.is_some() {
            return Err("legacy_approval_quarantined");
        }
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        let key = format!("mcp.approval.{id}");
        // A request made before the permissions file said deny is refused too.
        {
            let library = self.ready(&held)?;
            let request: Value = library
                .state(&key)
                .map_err(|_| "library_unavailable")?
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or(Value::Null);
            if let Some(tool) = request["tool"].as_str() {
                let active = permissions::load(library, &held.path, None)?;
                if gate_for(library, None, &active, tool, &request["args"])?.0 == Gate::Deny {
                    return Err(DENIED);
                }
            }
        }
        self.ready(&held)?
            .atomically(|library| {
                let raw = library.state(&key)?.ok_or_else(|| {
                    quince_core::Error::Io(std::io::Error::other("approval_not_found"))
                })?;
                let value: Value = serde_json::from_str(&raw).map_err(|_| {
                    quince_core::Error::Io(std::io::Error::other("approval_not_found"))
                })?;
                if value["account"] != json!(library.state(ACCOUNT_KEY)?) {
                    return Err(quince_core::Error::Io(std::io::Error::other(
                        "account_mismatch",
                    )));
                }
                let result = match value["tool"].as_str() {
                    Some("notes_write") => serde_json::from_value(value["args"].clone())
                        .map_err(|_| "invalid_arguments")
                        .and_then(|args| enqueue_note(library, args)),
                    Some("reminders_write") => serde_json::from_value(value["args"].clone())
                        .map_err(|_| "invalid_arguments")
                        .and_then(|args| enqueue_reminder(library, args)),
                    _ => Err("invalid_tool"),
                }
                .map_err(|e| quince_core::Error::Io(std::io::Error::other(e)))?;
                library.set_state(&key, "")?;
                Ok(result)
            })
            .map_err(|_| "approval_refused")
    }

    /// `skykeep-mcp approve`: runs a queued request once. A request made because the
    /// permissions file says ask: terminal runs through the same checks as any tool call; an
    /// older request goes through [`Self::approve`] as before.
    pub async fn approve_request(&mut self, id: &str) -> Result<Value, &'static str> {
        uuid::Uuid::parse_str(id).map_err(|_| "invalid_request_id")?;
        let value: Value = {
            let held = state::PrivateState::open(&self.dir)?;
            let raw = self
                .ready(&held)?
                .state(&format!("mcp.approval.{id}"))
                .map_err(|_| "library_unavailable")?
                .filter(|s| !s.is_empty())
                .ok_or("approval_not_found")?;
            serde_json::from_str(&raw).map_err(|_| "approval_not_found")?
        };
        if value.get("binding").is_none() {
            return self.approve(id);
        }
        let tool = value["tool"].as_str().ok_or("invalid_tool")?.to_string();
        self.terminal = Some(id.to_string());
        let result = self.call(&tool, value["args"].clone()).await;
        self.terminal = None;
        result
    }

    async fn call(&mut self, name: &str, args: Value) -> Result<Value, &'static str> {
        let grant = self.grant.take();
        let terminal = self.terminal.take();
        let drive_seam = self.drive.clone();
        let calendar_seam = self.calendar.clone();
        let contacts_seam = self.contacts.clone();
        let held = state::PrivateState::open(&self.dir)?;
        let dir = held.path.clone();
        let library = self.ready(&held)?;
        let configured = policy(library, &dir)?;
        let stored = stored_mode(library, &dir)?;
        let active = permissions::load(library, &dir, stored.as_deref())?;
        // Queued changes of a switched-off service never wait to be sent later.
        sync::set_aside_switched_off(library, &active.services)?;
        let configured = match configured {
            None if adopt_file_mode(library, &dir, &active)? => policy(library, &dir)?,
            configured => configured,
        };
        // The file's approval_mode, when it names one, replaces the mode setup stored.
        let mode = active.mode.clone();
        let (gate, explicit) = gate_for(library, mode.as_deref(), &active, name, &args)?;
        if gate == Gate::Deny {
            self.denied = Some(deny_rule(&active.rules, name, &args));
            return Err(DENIED);
        }
        let mut current = current_binding(library, configured.as_ref(), &active)?;
        current.identity = directory_identity(&dir)?;
        if let Some(ref grant) = grant {
            let mut expected = grant.clone();
            expected.snapshot = None;
            if current != expected {
                return Err("approval_policy_changed");
            }
            let mut exact = current.clone();
            if grant.snapshot.is_some() {
                exact.snapshot = Some(resolved_snapshot(library, name, &args, &active)?);
            }
            if grant != &exact {
                // A Photos card approved for the photo or album as it was cached, which
                // changed since: the same code as a stale terminal request, with what to do.
                if name == photos_write::TOOL && grant.snapshot != exact.snapshot {
                    return Err(photos_write::STALE);
                }
                return Err("approval_policy_changed");
            }
        }
        let policy_raw = configured.as_ref().map(|(_, b)| b.policy.clone());
        let terminal_approved = match &terminal {
            Some(id) => {
                terminal_check(
                    library,
                    id,
                    name,
                    &args,
                    policy_raw.as_deref(),
                    current.identity,
                    &active.fingerprint,
                )?;
                true
            }
            None => false,
        };
        match gate {
            Gate::Client if !terminal_approved => {
                let mut binding = current.clone();
                if name.ends_with("_write")
                    && (args["action"] != "create" || name == calendar_write::TOOL)
                {
                    binding.snapshot = Some(resolved_snapshot(library, name, &args, &active)?);
                }
                if grant.as_ref() != Some(&binding) {
                    return Err("approval_required_or_stale");
                }
            }
            // A Drive change gets Drive's own request, bound to its preview, below.
            Gate::Terminal
                if !terminal_approved
                    && name != drive_write::TOOL
                    && name != calendar_write::TOOL =>
            {
                // A Contacts request that can't run as asked (a version not cached, a card not
                // cached, a field shown cut short) is refused now, not stored to fail later.
                if name == contacts_write::TOOL {
                    contacts_write::preview(library, &args)?;
                }
                return terminal_proposal(
                    library,
                    &dir,
                    name,
                    &args,
                    policy_raw.as_deref(),
                    current.identity,
                    &active.fingerprint,
                )
                .map(|mut reply| {
                    if let Some(text) = active.fallback() {
                        reply["permissions_file"] = json!(text);
                    }
                    reply
                });
            }
            _ => {}
        }
        // A refresh needs approval under reads_and_writes; a rule or a card can give it.
        let approved = grant.is_some() || terminal_approved || gate == Gate::Run;
        let result = match name {
            "sync" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                sync::run_authorized(&dir, approved).await
            }
            "contacts_list" => contacts::list(library, args),
            "contacts_search" => contacts::search(library, args),
            "contacts_get" => contacts::get(library, args),
            "contacts_refresh" => {
                contacts::validate(name, &args)?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                contacts::refresh(&dir, approved).await
            }
            "calendar_calendars" => calendar::calendars(library, args),
            "calendar_events" => calendar::events(library, args),
            "calendar_search" => calendar::search(library, args),
            "calendar_get" => calendar::get(library, args),
            "calendar_refresh" => {
                calendar::validate(name, &args)?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                calendar::refresh(&dir, approved, args).await
            }
            "photos_albums" => photos::albums(library, args),
            "photos_list" => photos::list(library, args),
            "photos_search" => photos::search(library, args),
            "photos_get" => photos::get(library, args),
            "photos_refresh" => {
                photos::validate(name, &args)?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                photos::refresh(&dir, approved).await
            }
            "drive_list" => drive::list(library, args),
            "drive_get" => drive::get(library, args),
            "drive_search" => drive::search(library, args),
            "drive_refresh" => {
                drive::validate(name, &args)?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                drive::refresh(&dir, approved, args).await
            }
            "drive_write" => {
                let op = drive_write::validate(&args)?;
                let account = library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("login_required")?;
                // Only an ask: terminal rule waits for the terminal; a card the person approved
                // is that person's approval, with no second ask.
                if grant.is_none() && gate == Gate::Terminal {
                    let id = uuid::Uuid::new_v4().to_string();
                    let args = serde_json::to_value(&op).map_err(|_| "invalid_arguments")?;
                    let proposal = json!({"tool":name,"args":args,"account":account,
                        "policy":library.state("mcp.policy").map_err(|_| "policy_unavailable")?,
                        "state_identity":directory_identity(&dir)?,
                        "rules":active.fingerprint,
                        "preview":drive_write::preview(library, &args)?});
                    library
                        .set_state(&format!("mcp.approval.{id}"), &proposal.to_string())
                        .map_err(|_| "library_unavailable")?;
                    return Ok(json!({"status":"approval_required","request_id":id,
                        "approve_in_terminal":status::with_dir("skykeep-mcp approve", &dir),
                        "remote_verified":false}));
                }
                // An allow rule counts as approval only where Drive needs no person; run
                // checks that again itself.
                let allowed = gate == Gate::Run
                    && explicit == Some(permissions::Effect::Allow)
                    && !drive_write::needs_person(library, &op)?;
                drive_write::run(&dir, &drive_seam, op, grant.is_some() || allowed).await
            }
            "photos_write" => {
                let op = photos_write::validate(&args)?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                // A terminal request holds to the preview it showed, and is used up before it
                // runs, so it runs once.
                if let (true, Some(id)) = (terminal_approved, &terminal) {
                    let stored: Value = library
                        .state(&format!("mcp.approval.{id}"))
                        .map_err(|_| "library_unavailable")?
                        .and_then(|raw| serde_json::from_str(&raw).ok())
                        .unwrap_or(Value::Null);
                    if stored["preview"] != photos_write::preview(library, &args)? {
                        return Err(photos_write::STALE);
                    }
                    library
                        .set_state(&format!("mcp.approval.{id}"), "")
                        .map_err(|_| "library_unavailable")?;
                }
                // An allow rule counts as approval only where Photos needs no person.
                let allowed = gate == Gate::Run
                    && explicit == Some(permissions::Effect::Allow)
                    && !photos_write::needs_person(&op);
                photos_write::run(
                    &dir,
                    &drive_seam,
                    op,
                    grant.is_some() || terminal_approved || allowed,
                )
                .await
            }
            "calendar_write" => {
                let op = calendar_write::validate(&args)?;
                let account = library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("login_required")?;
                // As for Drive: only an ask: terminal rule waits for the terminal.
                if grant.is_none() && gate == Gate::Terminal {
                    let id = uuid::Uuid::new_v4().to_string();
                    let args = serde_json::to_value(&op).map_err(|_| "invalid_arguments")?;
                    let proposal = json!({"tool":name,"args":args,"account":account,
                        "policy":library.state("mcp.policy").map_err(|_| "policy_unavailable")?,
                        "state_identity":directory_identity(&dir)?,
                        "rules":active.fingerprint,
                        "preview":calendar_write::preview(library, &args, permissions::invitations_allowed(&active), true)?});
                    library
                        .set_state(&format!("mcp.approval.{id}"), &proposal.to_string())
                        .map_err(|_| "library_unavailable")?;
                    return Ok(json!({"status":"approval_required","request_id":id,
                        "approve_in_terminal":status::with_dir("skykeep-mcp approve", &dir),
                        "next":status::with_dir("Ask the person to run skykeep-mcp approve in their own terminal and type this request ID there.", &dir),
                        "remote_verified":false}));
                }
                let allowed = gate == Gate::Run
                    && explicit == Some(permissions::Effect::Allow)
                    && !calendar_write::is_destructive(op.action());
                let invitations = permissions::invitations_allowed(&active);
                calendar_write::run(
                    &dir,
                    &calendar_seam,
                    op,
                    grant.is_some() || allowed,
                    invitations,
                )
                .await
            }
            "contacts_write" => {
                let op = contacts_write::validate(&args)?;
                library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("login_required")?;
                // A delete runs only with a person's approval, whatever let it this far.
                if !(grant.is_some() || terminal_approved)
                    && (contacts_write::needs_person(name, &args)
                        || !matches!(gate, Gate::Run | Gate::Legacy))
                {
                    return Err("approval_required");
                }
                // A terminal request is used up before anything is sent, as for Drive, so a
                // second approve can't send it again after an unknown outcome.
                if let (true, Some(id)) = (terminal_approved, &terminal) {
                    library
                        .set_state(&format!("mcp.approval.{id}"), "")
                        .map_err(|_| "library_unavailable")?;
                }
                contacts_write::run(&dir, &contacts_seam, op).await
            }
            "sync_status" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let health = library.sync_health().map_err(|_| "library_unavailable")?;
                let refused = library
                    .outbox_refused_brief()
                    .map_err(|_| "library_unavailable")?;
                let mut services = Vec::new();
                for service in ["notes", "reminders"] {
                    let h = health.iter().find(|h| h.service == service);
                    let mine: Vec<_> = refused.iter().filter(|r| r.service == service).collect();
                    let pending = library
                        .outbox_pending(service)
                        .map_err(|_| "library_unavailable")?
                        .len();
                    // Upstream error text may hold HTTP bodies; report only that one exists.
                    services.push(
                        json!({"service":service,"switched_off":!active.service_on(service),"last_synced_at":h.and_then(|h| h.last_synced_at),
                        "last_error":h.is_some_and(|h| h.last_error.is_some()),"pending":pending,
                        "refused":mine.len(),
                        // Identifiers and reason codes only, newest last, so the user can be
                        // told which approved write will not be sent and why.
                        "refused_recent":mine[mine.len().saturating_sub(20)..].iter().map(|r| sync::refused_view(r)).collect::<Vec<_>>()}),
                    );
                }
                let mut reply =
                    json!({"source":"local_cache","services":services,"remote_verified":false});
                let off = active.services_off();
                if !off.is_empty() {
                    reply["switched_off"] = json!(off);
                }
                Ok(reply)
            }
            "auth_status" => {
                let _: Empty = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let bound = library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_some();
                // Every follow-up names this folder, so it can be typed as shown.
                let here = status::with_dir("skykeep-mcp setup", &dir);
                if let Some(closed) = storage::protected_problem(&dir) {
                    let text = status::with_dir(storage::tool_text(&closed), &dir);
                    return Ok(
                        json!({"status":if closed == storage::Closed::Locked {"locked"} else {"storage_problem"},"instructions":format!("{text}. Open your own trusted terminal, outside agent/tool capture, and type any passphrase there. Never send passwords, codes or passphrases to MCP or chat."),"session_checked":false}),
                    );
                }
                Ok(
                    json!({"status":if bound {"session_unverified"} else {"login_required"},"instructions":format!("Run {here} in your own trusted terminal, outside agent/tool capture. Never send passwords or codes to MCP or chat."),"session_checked":false}),
                )
            }
            "reminders_get" => {
                let args: Get = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let item = library
                    .reminder(&args.guid)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("not_found")?;
                let detail = library
                    .reminder_detail(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                let mut reply = preserve::reminder_report(library, &item)?;
                reply["source"] = json!("local_cache");
                reply["item"] = json!(item);
                reply["detail"] = json!(detail);
                reply["complete_representation"] = json!(false);
                Ok(reply)
            }
            "reminders_lists" => {
                let args: Page = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .reminder_lists()
                        .map_err(|_| "library_unavailable")?,
                    args.into(),
                )
            }
            "notes_folders" => {
                let args: Page = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library.note_folders().map_err(|_| "library_unavailable")?,
                    args.into(),
                )
            }
            "notes_get" => {
                let args: Get = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let mut item = library
                    .note(&args.guid)
                    .map_err(|_| "library_unavailable")?
                    .ok_or("not_found")?;
                item.body = library
                    .note_body(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                item.runs = library
                    .note_runs(&args.guid)
                    .map_err(|_| "library_unavailable")?;
                let runs: Option<Value> = item
                    .runs
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|_| "invalid_cached_runs")?;
                let mut reply = preserve::note_report(library, &item, item.runs.as_deref())?;
                reply["source"] = json!("local_cache");
                reply["item"] = json!(item);
                reply["runs"] = json!(runs);
                reply["complete_representation"] = json!(false);
                Ok(reply)
            }
            "reminders_search" => {
                let args: Search = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .search_reminders(&args.query)
                        .map_err(|_| "search_unavailable")?,
                    List {
                        offset: args.offset,
                        limit: args.limit,
                        parent: None,
                    },
                )
            }
            "notes_search" => {
                let args: Search = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                page(
                    library
                        .search_notes(&args.query)
                        .map_err(|_| "search_unavailable")?,
                    List {
                        offset: args.offset,
                        limit: args.limit,
                        parent: None,
                    },
                )
            }
            "notes_list" => {
                let args: List = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let items = library
                    .notes_in(args.parent.as_deref())
                    .map_err(|_| "library_unavailable")?;
                page(items, args)
            }
            "reminders_list" => {
                let args: List = serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                let parent = args.parent.as_deref().ok_or("parent_required")?;
                let items = library
                    .reminders_in(parent, true)
                    .map_err(|_| "library_unavailable")?;
                page(items, args)
            }
            "reminders_write" => {
                let args: ReminderWrite =
                    serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                if configured.is_some() {
                    authorized_enqueue(
                        library,
                        &dir,
                        mode.as_deref(),
                        once(&terminal, |l| enqueue_reminder(l, args)),
                    )
                } else {
                    in_one(library, once(&terminal, |l| enqueue_reminder(l, args)))
                }
            }
            "notes_write" => {
                let args: NoteWrite =
                    serde_json::from_value(args).map_err(|_| "invalid_arguments")?;
                if library
                    .state(ACCOUNT_KEY)
                    .map_err(|_| "library_unavailable")?
                    .is_none()
                {
                    return Err("login_required");
                }
                if configured.is_some() {
                    authorized_enqueue(
                        library,
                        &dir,
                        mode.as_deref(),
                        once(&terminal, |l| enqueue_note(l, args)),
                    )
                } else {
                    in_one(library, once(&terminal, |l| enqueue_note(l, args)))
                }
            }
            _ => Err("unknown_tool"),
        };
        if terminal_approved && result.is_ok() {
            if let Some(id) = &terminal {
                library
                    .set_state(&format!("mcp.approval.{id}"), "")
                    .map_err(|_| "library_unavailable")?;
            }
        }
        let mut result = result;
        if let Some(value) = result.as_mut().ok().filter(|v| v.is_object()) {
            if let Some(text) = active.fallback() {
                value["permissions_file"] = json!(text);
            }
            // Said once: changes set aside when the file's mode was taken over.
            let set_aside = library
                .state(SET_ASIDE_KEY)
                .map_err(|_| "library_unavailable")?
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|n| *n > 0);
            if let Some(n) = set_aside {
                library
                    .set_state(SET_ASIDE_KEY, "")
                    .map_err(|_| "library_unavailable")?;
                value["set_aside"] = json!(format!(
                    "{n} waiting change{} set aside when the approval mode from permissions.yaml was first stored; none will be sent. To see them, run {} in your own terminal.",
                    if n == 1 { " was" } else { "s were" },
                    status::with_dir("skykeep-mcp quarantine", &dir)
                ));
            }
        }
        result
    }
}

/// `enqueue`, and in the same transaction the clearing of the terminal request it answers, so
/// an approved request runs at most once even if the process stops halfway.
fn once<'a>(
    terminal: &'a Option<String>,
    enqueue: impl FnOnce(&mut Library) -> Result<Value, &'static str> + 'a,
) -> impl FnOnce(&mut Library) -> Result<Value, &'static str> + 'a {
    move |library| {
        let result = enqueue(library)?;
        if let Some(id) = terminal {
            library
                .set_state(&format!("mcp.approval.{id}"), "")
                .map_err(|_| "library_unavailable")?;
        }
        Ok(result)
    }
}

/// `apply` in one transaction: everything it wrote is undone when it fails.
fn in_one(
    library: &mut Library,
    apply: impl FnOnce(&mut Library) -> Result<Value, &'static str>,
) -> Result<Value, &'static str> {
    struct Refused(&'static str);
    impl From<quince_core::Error> for Refused {
        fn from(_: quince_core::Error) -> Self {
            Refused("library_unavailable")
        }
    }
    library
        .atomically(|library| apply(library).map_err(Refused))
        .map_err(|Refused(error)| error)
}

/// `mode` is the approval mode the call's gate decided with; each write is recorded under it.
fn authorized_enqueue(
    library: &mut Library,
    dir: &Path,
    mode: Option<&str>,
    enqueue: impl FnOnce(&mut Library) -> Result<Value, &'static str>,
) -> Result<Value, &'static str> {
    if library
        .outbox_refused_count()
        .map_err(|_| "library_unavailable")?
        >= sync::MAX_REFUSED
    {
        return Err("quarantine_full_retire_in_terminal");
    }
    // Writes queued under a looser mode than the one in force are set aside first.
    sync::quarantine_looser(library, dir)?;
    let before = sync::pending_operations(library)?;
    for p in &before {
        sync::authorized_payload(library, dir, p)?;
    }
    let exists = |library: &Library, p: &quince_core::outbox::Pending| {
        if p.service == "notes" {
            library.note(&p.record).map(|n| n.is_some())
        } else {
            library.reminder(&p.record).map(|r| r.is_some())
        }
    };
    // Writes whose full grant, context included, is current right now. Only this approved
    // write can change their context before the transaction ends, so they may be re-bound.
    let mut current = Vec::new();
    let mut present = Vec::new();
    for p in &before {
        if sync::authorized(library, dir, p).is_ok() {
            current.push(p.id);
        }
        if exists(library, p).map_err(|_| "library_unavailable")? {
            present.push(p.id);
        }
    }
    // A refusal from the write keeps its own code; a library failure is `mutation_refused`.
    struct Refused(&'static str);
    impl From<quince_core::Error> for Refused {
        fn from(_: quince_core::Error) -> Self {
            Refused("mutation_refused")
        }
    }
    library
        .atomically(|library| -> Result<Value, Refused> {
            let result = enqueue(library).map_err(Refused)?;
            let after = sync::pending_operations(library).map_err(Refused)?;
            for p in &after {
                let old = before.iter().find(|old| old.id == p.id);
                if old.is_some()
                    && p.kind != "delete"
                    && present.contains(&p.id)
                    && !exists(library, p)?
                {
                    // This approved deletion removed the item, or its parent took it along.
                    // Older writes to it are superseded, never sent with an invented context.
                    library.outbox_refuse(p.id, "superseded_by_approved_deletion")?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                    continue;
                }
                // A new operation from this write, or any queued operation on the record
                // the human just reviewed (the preview lists them), takes a fresh grant. An
                // unchanged write elsewhere is re-bound only if its grant was current a
                // moment ago, so only this approved write moved its context. A changed
                // payload on another record is never re-authorized; sync quarantines it.
                if old.is_none()
                    || result["guid"] == p.record
                    || (old == Some(p) && current.contains(&p.id))
                {
                    sync::record_authorization(library, dir, p, mode).map_err(Refused)?;
                }
            }
            for p in &before {
                if !after.iter().any(|new| new.id == p.id) {
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")?;
                }
            }
            Ok(result)
        })
        .map_err(|Refused(error)| error)
}

/// One-time setup under the directory lock: persistent document writer and private modes.
fn setup(path: &Path) -> Result<Library, &'static str> {
    let mut library = Library::open(path).map_err(|_| "library_unavailable")?;
    let id = library
        .state("documents.writer")
        .map_err(|_| "library_unavailable")?
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        .unwrap_or_else(uuid::Uuid::new_v4);
    library
        .set_state("documents.writer", &id.to_string())
        .map_err(|_| "library_unavailable")?;
    quince_icloud::topotext::set_writer(*id.as_bytes());
    use std::os::unix::fs::PermissionsExt;
    for file in std::fs::read_dir(path).map_err(|_| "state_unavailable")? {
        let file = file.map_err(|_| "state_unavailable")?;
        // The permissions file keeps the mode its owner gave it; its reader reports a shared one.
        if file.file_type().map_err(|_| "state_unavailable")?.is_file()
            && file.file_name() != permissions::FILE_NAME
        {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "state_unavailable")?;
        }
    }
    Ok(library)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    guid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct List {
    parent: Option<String>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

/// Arguments of list tools that have no parent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

impl From<Page> for List {
    fn from(p: Page) -> Self {
        List {
            parent: None,
            offset: p.offset,
            limit: p.limit,
        }
    }
}

fn page<T: serde::Serialize>(items: Vec<T>, args: List) -> Result<Value, &'static str> {
    let total = items.len();
    let limit = args.limit.unwrap_or(100).clamp(1, 1000);
    let items: Vec<_> = items.into_iter().skip(args.offset).take(limit).collect();
    let next = args.offset.saturating_add(items.len());
    Ok(
        json!({"source":"local_cache","items":items,"total":total,"next_offset":(next < total).then_some(next),"complete_representation":false}),
    )
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum NoteWrite {
    Move {
        guid: String,
        folder_guid: Option<String>,
        base_etag: Option<String>,
    },
    Pin {
        guid: String,
        pinned: bool,
        base_etag: Option<String>,
    },
    Save {
        guid: String,
        body: String,
        base_etag: Option<String>,
        runs: Option<Vec<quince_icloud::ckdocument::Run>>,
    },
    Create {
        body: String,
        folder_guid: Option<String>,
    },
    Delete {
        guid: String,
        base_etag: Option<String>,
    },
}

/// `None` files a note in the default folder; a named folder must be in the synced cache.
fn require_folder(library: &Library, folder: Option<&str>) -> Result<(), &'static str> {
    let Some(folder) = folder else { return Ok(()) };
    if library
        .note_folders()
        .map_err(|_| "library_unavailable")?
        .iter()
        .any(|f| f.guid == folder)
    {
        Ok(())
    } else {
        Err("folder_not_found")
    }
}

fn enqueue_note(library: &mut Library, args: NoteWrite) -> Result<Value, &'static str> {
    match args {
        NoteWrite::Move {
            guid,
            folder_guid,
            base_etag,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            require_folder(library, folder_guid.as_deref())?;
            quince_sync::notes::record_note_move(library, &guid, folder_guid)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Pin {
            guid,
            pinned,
            base_etag,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            quince_sync::notes::record_note_pin(library, &guid, pinned)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Save {
            guid,
            body,
            base_etag,
            runs,
        } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            let base = library
                .note_body(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("note_body_unavailable_sync_first")?;
            let base_runs = library
                .note_runs(&guid)
                .map_err(|_| "library_unavailable")?
                .map(|raw| serde_json::from_str(&raw).map_err(|_| "invalid_cached_runs"))
                .transpose()?;
            preserve::check_note_save(&base, base_runs.as_deref(), &body, runs.as_deref())?;
            if let Some(sent_from) = preserve::send_base(library, &guid)? {
                preserve::keeps_attachment_marks(&sent_from, &body)?;
            }
            let kept_attachments = body.chars().filter(|&c| c == '\u{FFFC}').count();
            let formatting = if runs.is_some() {
                "restyled_from_runs"
            } else {
                "kept_from_apple"
            };
            quince_sync::notes::record_note_save(
                library,
                &guid,
                &body,
                Some(base),
                runs,
                base_runs,
            )
            .map_err(|_| "write_refused")?;
            Ok(
                json!({"guid":guid,"status":"queued","remote_verified":false,"kept_attachments":kept_attachments,"formatting":formatting}),
            )
        }
        NoteWrite::Delete { guid, base_etag } => {
            let note = library
                .note(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if note.etag != base_etag {
                return Err("conflict");
            }
            if note.locked || !note.editable {
                return Err("read_only_note");
            }
            quince_sync::notes::record_note_delete(library, &note).map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        NoteWrite::Create { body, folder_guid } => {
            if folder_guid.as_deref() == Some(quince_core::notes::TRASH_FOLDER) {
                return Err("folder_not_writable");
            }
            require_folder(library, folder_guid.as_deref())?;
            let guid = quince_sync::notes::record_note_create(library, folder_guid, &body)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
    }
}

/// `Option<Option<T>>` kept whole through JSON: absent is `None`, null is `Some(None)`.
mod present {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer, T: Serialize>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ReminderWrite {
    Delete {
        guid: String,
    },
    Tick {
        guid: String,
        done: bool,
    },
    Edit {
        guid: String,
        patch: Value,
        /// Absent: not checked. Present: must be the cached change tag, `null` for none.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "present")]
        base_etag: Option<Option<String>>,
    },
    Create {
        list_guid: String,
        title: String,
        due_at: Option<i64>,
        all_day: Option<bool>,
        flagged: Option<bool>,
        // Left out of the approval record when unset, so a plain create reads as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repeat: Option<RepeatArg>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_time_alert: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        early_alert: Option<EarlyAlertArg>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

#[derive(Deserialize, serde::Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RepeatFrequency {
    Daily,
}

/// A new reminder's repeat: daily, until the end of `until` (`YYYY-MM-DD`) or for ever.
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RepeatArg {
    frequency: RepeatFrequency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until: Option<String>,
}

#[derive(Deserialize, serde::Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AlertUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
}

impl AlertUnit {
    /// Apple's `dueDateDeltaUnit`.
    fn apple(self) -> i64 {
        match self {
            AlertUnit::Minutes => 0,
            AlertUnit::Hours => 1,
            AlertUnit::Days => 2,
            AlertUnit::Weeks => 3,
            AlertUnit::Months => 4,
        }
    }
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct EarlyAlertArg {
    amount: i64,
    unit: AlertUnit,
}

/// The largest early-alert amount Apple's writer takes.
const MOST_EARLY_ALERT: i64 = 10_000;

/// A create's repeat and alerts, checked against its date and turned into what is queued, with
/// every record id chosen now. Refused with a code naming the first combination that does
/// not work; nothing is queued then.
struct CreateOptions {
    timezone: Option<String>,
    repeat: Option<quince_sync::reminders::CreateRepeat>,
    alarm: Option<quince_sync::reminders::CreateAlarm>,
    early_alert: Option<quince_sync::reminders::CreateEarlyAlert>,
}

fn part_id() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

#[allow(clippy::too_many_arguments)]
fn create_options(
    library: &Library,
    due_at: Option<i64>,
    all_day: Option<bool>,
    repeat: Option<RepeatArg>,
    due_time_alert: Option<bool>,
    early_alert: Option<EarlyAlertArg>,
    timezone: Option<String>,
) -> Result<CreateOptions, &'static str> {
    use quince_sync::reminders as r;
    let all_day = all_day.unwrap_or(false) && due_at.is_some();
    let timed = due_at.is_some() && !all_day;
    let timezone = match timezone {
        Some(name) => Some(r::iana_zone(&name).ok_or("unknown_timezone")?),
        None => None,
    };
    if timezone.is_some() && !timed {
        return Err("timezone_requires_timed_due_at");
    }
    // A timed create with options is kept in one zone from here on: chosen now, queued, and
    // used for its repeat's end and by every later send, whatever this computer's zone becomes.
    let any_option = repeat.is_some() || due_time_alert == Some(true) || early_alert.is_some();
    let timezone = match timezone {
        None if timed && any_option => Some(r::system_zone()),
        other => other,
    };
    let alarm = if due_time_alert == Some(true) {
        if due_at.is_none() {
            return Err("due_time_alert_requires_due_at");
        }
        if all_day {
            return Err("all_day_cannot_have_timed_alert");
        }
        Some(r::CreateAlarm {
            alarm_id: part_id(),
            trigger_id: part_id(),
        })
    } else {
        None
    };
    let early_alert = match early_alert {
        None => None,
        Some(alert) => {
            if !(1..=MOST_EARLY_ALERT).contains(&alert.amount) {
                return Err("invalid_early_alert");
            }
            if due_at.is_none() {
                return Err("early_alert_requires_due_at");
            }
            if all_day {
                return Err("all_day_cannot_have_timed_alert");
            }
            let account = library
                .early_alert_account()
                .map_err(|_| "library_unavailable")?
                .ok_or("early_alert_account_unavailable_set_one_on_an_apple_device_then_sync")?;
            Some(r::CreateEarlyAlert {
                count: alert.amount,
                unit: alert.unit.apple(),
                account,
                id: part_id(),
            })
        }
    };
    let repeat = match repeat {
        None => None,
        Some(repeat) => {
            let due = due_at.ok_or("repeat_requires_due_at")?;
            let RepeatFrequency::Daily = repeat.frequency;
            let end_at = match repeat.until {
                None => None,
                Some(until) => {
                    // An all-day date is that day's midnight in UTC, so its last day is read
                    // there; a timed one in the zone it rings in.
                    let zone = if all_day {
                        "UTC".to_string()
                    } else {
                        timezone.clone().unwrap_or_else(r::system_zone)
                    };
                    let end = r::end_of_day(&until, &zone).ok_or("invalid_repeat_end")?;
                    if end < due {
                        return Err("repeat_end_before_due");
                    }
                    Some(end)
                }
            };
            Some(r::CreateRepeat {
                frequency: 0,
                interval: 1,
                end_at,
                rule_id: part_id(),
            })
        }
    };
    Ok(CreateOptions {
        timezone,
        repeat,
        alarm,
        early_alert,
    })
}

/// The options a queued create carries, as the reply names them: no record ids or account id.
fn options_view(c: &quince_sync::reminders::Create) -> Value {
    let unit = |u: i64| match u {
        0 => "minutes",
        1 => "hours",
        2 => "days",
        3 => "weeks",
        _ => "months",
    };
    json!({
        "timezone": c.timezone,
        "repeat": c.repeat.as_ref().map(|r| json!({"frequency":"daily","ends_at":r.end_at})),
        "due_time_alert": c.alarm.is_some(),
        "early_alert": c.early_alert.as_ref().map(|e| json!({"amount":e.count,"unit":unit(e.unit)})),
    })
}

fn enqueue_reminder(library: &mut Library, args: ReminderWrite) -> Result<Value, &'static str> {
    match args {
        ReminderWrite::Delete { guid } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            quince_sync::reminders::record_reminder_delete(library, &reminder)
                .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        ReminderWrite::Tick { guid, done } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            quince_sync::reminders::record_reminder_tick(
                library,
                &reminder,
                done,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "clock_unavailable")?
                    .as_secs() as i64,
            )
            .map_err(|_| "write_refused")?;
            Ok(json!({"guid":guid,"status":"queued","remote_verified":false}))
        }
        ReminderWrite::Edit {
            guid,
            patch,
            base_etag,
        } => {
            let reminder = library
                .reminder(&guid)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if base_etag.as_ref().is_some_and(|tag| *tag != reminder.etag) {
                return Err("conflict");
            }
            let mut fields = patch.as_object().cloned().ok_or("invalid_patch")?;
            if fields.is_empty()
                || fields
                    .keys()
                    .any(|k| !preserve::REMINDER_FIELDS.contains(&k.as_str()))
            {
                return Err("invalid_patch");
            }
            preserve::check_reminder_edit(&reminder, &fields)?;
            let changed: Vec<&str> = preserve::REMINDER_FIELDS
                .into_iter()
                .filter(|f| fields.contains_key(*f))
                .collect();
            let alarms = preserve::reminder_alarm_effect(&fields);
            let conflict_check = if base_etag.is_some() {
                "base_etag"
            } else {
                "none"
            };
            fields.insert("from_list".into(), json!(reminder.list_guid));
            let e: quince_sync::reminders::Edit =
                serde_json::from_value(Value::Object(fields)).map_err(|_| "invalid_patch")?;
            let edit = quince_icloud::reminders::ReminderEdit {
                title: e.title,
                notes: e.notes,
                due_at: e.due_at,
                all_day: e.all_day,
                flagged: e.flagged,
                completed: e.completed,
                priority: e.priority,
                list_guid: e.list_guid,
                parent_guid: e.parent_guid,
                timezone: None,
            };
            quince_sync::reminders::record_reminder_edit(library, &reminder, &edit)
                .map_err(|_| "write_refused")?;
            Ok(
                json!({"guid":guid,"status":"queued","remote_verified":false,"changed_fields":changed,"conflict_check":conflict_check,"alarms":alarms}),
            )
        }
        ReminderWrite::Create {
            list_guid,
            title,
            due_at,
            all_day,
            flagged,
            repeat,
            due_time_alert,
            early_alert,
            timezone,
        } => {
            if !library
                .reminder_lists()
                .map_err(|_| "library_unavailable")?
                .iter()
                .any(|l| l.guid == list_guid)
            {
                return Err("list_unavailable_or_read_only");
            }
            if title.trim().is_empty() {
                return Err("title_required");
            }
            let options = create_options(
                library,
                due_at,
                all_day,
                repeat,
                due_time_alert,
                early_alert,
                timezone,
            )?;
            let create = quince_sync::reminders::Create {
                list_guid,
                title,
                due_at,
                all_day,
                flagged,
                timezone: options.timezone,
                repeat: options.repeat,
                alarm: options.alarm,
                early_alert: options.early_alert,
            };
            let guid = quince_sync::reminders::record_reminder_create(library, &create)
                .map_err(|_| "write_refused")?;
            // Queued here only. `sync` says which queued writes CloudKit took; nothing reads
            // the repeat or alerts back from Apple, so remote_verified stays false.
            Ok(
                json!({"guid":guid,"status":"queued","queued":true,"cloudkit_accepted":false,"remote_verified":false,"options":options_view(&create)}),
            )
        }
    }
}

const SERVER_TITLE: &str =
    "Skykeep: Apple iCloud Notes, Reminders, Contacts, Calendar, Photos & iCloud Drive";
const SERVER_INSTRUCTIONS: &str = "Skykeep reads and changes the user's Apple iCloud Notes and Apple Reminders through a local synced cache, reads the user's Apple iCloud Contacts and changes one card at a time with contacts_write, reads Apple iCloud Calendar and changes its events with calendar_write, reads Apple iCloud Photos metadata and changes it with photos_write (upload from the exchange folder, favorite, title and caption, rating, keywords, date, new and renamed albums, move to Recently Deleted), and reads and changes Apple iCloud Drive: folder listings and file details from the cache, and drive_write to create folders, rename, move, trash, delete for good, upload from and download into the exchange folder the user set up in their terminal. Reads return cached data. Contacts refresh only through contacts_refresh, Calendar only through calendar_refresh, Photos only through photos_refresh, and Drive one folder at a time through drive_refresh. Photos tools show the personal library's photo details, never the pictures, places or faces; the Photos tools other than photos_write are read-only. Drive paths start with / and use the names Apple shows. Calendar dates are YYYY-MM-DD; pass the user's IANA time zone to calendar_events. Changes follow the configured approval mode. Notes and Reminders changes are queued until sync sends them; a Drive, Contacts, Calendar or Photos change runs once it is allowed and its reply says what Apple confirmed. calendar_write needs the etag from calendar_get, an offset or IANA zone for every time, and scope for a repeating event. Sign-in happens only in the user's own terminal; never ask for Apple passwords or codes.";

/// The tools offered to the client: those of services that are on (all of them when nothing
/// can say, see [`Server::services_now`]), with `sync` described for what it refreshes.
fn listed_tools(services: Option<&[&str]>) -> Vec<Value> {
    let on = |tool: &str| services.is_none_or(|s| permissions::tool_on(s, tool));
    let mut tools: Vec<Value> = tools()
        .into_iter()
        .filter(|t| on(t["name"].as_str().unwrap_or_default()))
        .collect();
    let refreshes = match services.map(|s| (s.contains(&"notes"), s.contains(&"reminders"))) {
        Some((true, false)) => Some("Apple iCloud Notes"),
        Some((false, true)) => Some("Apple Reminders"),
        _ => None,
    };
    if let (Some(what), Some(sync)) = (refreshes, tools.iter_mut().find(|t| t["name"] == "sync")) {
        sync["description"] = json!(format!("Refresh {what} and send its approved queued changes. Queued/sent is not field-level remote verification. Never opens a login prompt."));
    }
    tools
}

/// Every tool, whether its service is on or not: rule names in the permissions file come from
/// this list, so a rule for a switched-off service stays valid.
pub(crate) fn tools() -> Vec<Value> {
    // (name, description, parent property, parent required)
    let lists = [
        (
            "notes_list",
            "List the user's Apple iCloud Notes from the local cache.",
            Some("Folder GUID; omit for all notes."),
            false,
        ),
        (
            "reminders_list",
            "List the user's Apple Reminders in one list from the local cache.",
            Some("Reminder list GUID."),
            true,
        ),
        (
            "notes_folders",
            "List Apple iCloud Notes folders from the local cache.",
            None,
            false,
        ),
        (
            "reminders_lists",
            "List Apple Reminders lists from the local cache.",
            None,
            false,
        ),
    ];
    let mut tools: Vec<_> = lists.iter().map(|(name, description, parent, required)| {
        let mut properties = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}});
        if let Some(description) = parent {
            properties["parent"] = json!({"type":"string","description":description});
        }
        let mut schema = json!({"type":"object","properties":properties,"additionalProperties":false});
        if *required {
            schema["required"] = json!(["parent"]);
        }
        json!({
            "name":name,"description":format!("{description} Sync requires an authenticated session."),
            "inputSchema":schema,
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        })
    }).collect();
    for (name, kind) in [
        ("notes_search", "Apple iCloud Notes"),
        ("reminders_search", "Apple Reminders"),
    ] {
        tools.push(json!({"name":name,"description":format!("Search the user's {kind} in the local cache index."),"inputSchema":{"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000}},"required":["query"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    }
    tools.push(json!({"name":"notes_write","description":"Queue a change to the user's Apple iCloud Notes (create, save, move, pin, delete). Follows the approval mode; a delete, a move into Recently Deleted or a save that empties a note always asks the user on the approval card. Queued is not remote-verified.","inputSchema":{"type":"object","properties":{"action":{"enum":["create","save","move","pin","delete"]},"body":{"type":"string"},"pinned":{"type":"boolean"},"runs":{"type":["array","null"],"items":{"type":"object"}},"folder_guid":{"type":["string","null"]},"guid":{"type":"string"},"base_etag":{"type":["string","null"],"description":"Needed for move, save, pin and delete: the etag from notes_get. A missing or older etag returns conflict."}},"required":["action"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"reminders_write","description":"Queue a change to the user's Apple Reminders (create, edit, tick, delete). A create can also repeat daily and alert at or before its due time. Deletions always ask the user on the approval card. Queued is not CloudKit-accepted; sync reports accepted writes. No field-level remote verification.","inputSchema":{"type":"object","properties":{"action":{"enum":["create","edit","tick","delete"]},"guid":{"type":"string"},"list_guid":{"type":"string"},"title":{"type":"string"},"due_at":{"type":["integer","null"]},"all_day":{"type":"boolean"},"flagged":{"type":"boolean"},"done":{"type":"boolean"},"patch":{"type":"object","description":"Supported fields: title, notes, due_at, all_day, flagged, completed, priority, list_guid, parent_guid. Absent leaves unchanged; null clears notes/due_at/parent_guid."},"base_etag":{"type":["string","null"],"description":"Edit only: the etag from reminders_get. A newer cached copy returns conflict."},"repeat":{"type":"object","description":"Create only. Repeats daily from due_at, until the end of the given day or for ever. Needs due_at.","properties":{"frequency":{"enum":["daily"]},"until":{"type":"string","pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}$","description":"YYYY-MM-DD, the last day it repeats. Read in timezone for a timed due_at, in UTC for an all-day one."}},"required":["frequency"],"additionalProperties":false},"due_time_alert":{"type":"boolean","description":"Create only. Alert at due_at. Needs a timed (not all-day) due_at."},"early_alert":{"type":"object","description":"Create only. Alert this long before due_at. Needs a timed due_at, and an early alert set on one of the user's reminders from an Apple device and then a sync, so the account id it carries is in the cache.","properties":{"amount":{"type":"integer","minimum":1,"maximum":10000},"unit":{"enum":["minutes","hours","days","weeks","months"]}},"required":["amount","unit"],"additionalProperties":false},"timezone":{"type":"string","maxLength":64,"description":"Create only. IANA zone the timed due_at and its alerts are kept in, such as Europe/Berlin. Default: this computer's zone, or UTC if it has no IANA name."}},"required":["action"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"reminders_get","description":"Read one cached Apple Reminders item and its supported details. Not a complete archive.","inputSchema":{"type":"object","properties":{"guid":{"type":"string"}},"required":["guid"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools.push(json!({"name":"notes_get","description":"Read one cached Apple iCloud Notes note. Not a complete preservation archive.","inputSchema":{"type":"object","properties":{"guid":{"type":"string"}},"required":["guid"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools.push(json!({"name":"sync","description":"Refresh Apple iCloud Notes and Apple Reminders and send approved queued changes. Queued/sent is not field-level remote verification. Never opens a login prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}}));
    tools.push(json!({"name":"sync_status","description":"Local sync state for Apple iCloud Notes and Reminders: last refresh, whether the last pass failed, pending and refused queued changes. No network access.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    let page = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":contacts::MAX_LIMIT,"default":25}});
    let read = json!({"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false});
    tools.push(json!({"name":"contacts_list","description":"List the user's Apple iCloud Contacts from the local cache, in name order, as summaries with emails and phones. Read-only; refresh with contacts_refresh.","inputSchema":{"type":"object","properties":page,"additionalProperties":false},"annotations":read}));
    let mut search = page.clone();
    search["query"] = json!({"type":"string","minLength":1,"maxLength":contacts::MAX_QUERY_BYTES,"description":"Words matched against names, company, details, emails, phones and other cached fields, ignoring case and accents. At most 256 UTF-8 bytes."});
    tools.push(json!({"name":"contacts_search","description":"Search the user's cached Apple iCloud Contacts by name, email, phone or other kept fields. Read-only.","inputSchema":{"type":"object","properties":search,"required":["query"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"contacts_get","description":"Read one cached Apple iCloud Contacts card with its kept fields and lists. Not a complete card: photos, the original vCard and fields the cache does not keep are not included.","inputSchema":{"type":"object","properties":{"guid":{"type":"string","minLength":1,"maxLength":512,"description":"Contact GUID from contacts_list or contacts_search. At most 512 UTF-8 bytes."}},"required":["guid"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"contacts_refresh","description":"Read the user's Apple iCloud Contacts from Apple into the local cache, replacing the cached copy. Never changes contacts at Apple and sends no queued changes. Never opens a login prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}}));
    let page = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":calendar::MAX_LIMIT,"default":25}});
    let day = |what: &str| json!({"type":"string","pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}$","description":what});
    let guid = json!({"type":"string","minLength":1,"maxLength":calendar::MAX_GUID_BYTES});
    tools.push(json!({"name":"calendar_calendars","description":"List the user's Apple iCloud Calendar calendars from the local cache, with which is the default and which are read-only. Read-only; refresh with calendar_refresh.","inputSchema":{"type":"object","properties":page,"additionalProperties":false},"annotations":read}));
    let mut range = page.clone();
    range["from"] = day("First day, YYYY-MM-DD, in timezone.");
    range["to"] = day(&format!(
        "Day after the last, YYYY-MM-DD (exclusive). At most {} days after from.",
        calendar::MAX_RANGE_DAYS
    ));
    range["timezone"] = json!({"type":"string","minLength":1,"maxLength":calendar::MAX_ZONE_BYTES,"description":"IANA time zone the days are read in, such as Europe/Berlin."});
    range["calendar_guid"] = guid.clone();
    tools.push(json!({"name":"calendar_events","description":"List the user's Apple iCloud Calendar events from the local cache that overlap a date range. All-day events are dates and floating events are clock times; timed events give UTC and zone times. Says whether the last calendar_refresh covered the range. Read-only.","inputSchema":{"type":"object","properties":range,"required":["from","to","timezone"],"additionalProperties":false},"annotations":read}));
    let mut search = page.clone();
    search["query"] = json!({"type":"string","minLength":1,"maxLength":calendar::MAX_QUERY_BYTES,"description":"Words matched against title, location and notes, ignoring case and accents. At most 256 UTF-8 bytes."});
    tools.push(json!({"name":"calendar_search","description":"Search the user's cached Apple iCloud Calendar events by title, location or notes. At most 300 matches nearest today are kept. Read-only.","inputSchema":{"type":"object","properties":search,"required":["query"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"calendar_get","description":"Read one cached Apple iCloud Calendar event with its notes and calendar. Not a complete event: attendees, alerts, the repeat rule and status are not included.","inputSchema":{"type":"object","properties":{"guid":guid},"required":["guid"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"calendar_refresh","description":format!("Read the user's Apple iCloud Calendar from Apple into the local cache over a window (default: last month to three months ahead; at most {} days). Never changes events at Apple, never answers invitations and sends no queued changes. Never opens a login prompt.", calendar::MAX_REFRESH_DAYS),"inputSchema":{"type":"object","properties":{"from":day("First day, YYYY-MM-DD (UTC)."),"to":day("Day after the last, YYYY-MM-DD (exclusive).")},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}}));
    let page = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":photos::MAX_LIMIT,"default":25}});
    let id = json!({"type":"string","minLength":1,"maxLength":photos::MAX_ID_BYTES});
    tools.push(json!({"name":"photos_albums","description":"List the albums and folders the user made in Apple iCloud Photos, from the local cache, with how many photos each shows. Read-only; refresh with photos_refresh.","inputSchema":{"type":"object","properties":page,"additionalProperties":false},"annotations":read}));
    let mut list = page.clone();
    list["album_id"] = json!({"type":"string","minLength":1,"maxLength":photos::MAX_ID_BYTES,"description":"Album id from photos_albums; omit for the whole library."});
    tools.push(json!({"name":"photos_list","description":"List the user's Apple iCloud Photos (personal library) from the local cache, newest first or in an album's order: dates, kind (image, video, Live Photo), size, filename, title, caption and keywords. No pictures, places or faces. Read-only.","inputSchema":{"type":"object","properties":list,"additionalProperties":false},"annotations":read}));
    let mut search = page.clone();
    search["query"] = json!({"type":"string","minLength":1,"maxLength":photos::MAX_QUERY_BYTES,"description":"Words matched against title, filename, caption, keywords, and the year or month taken. At most 256 UTF-8 bytes."});
    tools.push(json!({"name":"photos_search","description":"Search the user's cached Apple iCloud Photos (personal library) by title, filename, caption, keyword or the year or month taken. Read-only.","inputSchema":{"type":"object","properties":search,"required":["query"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"photos_get","description":"Read one cached Apple iCloud Photos item's details, with a longer caption. Not the picture itself: no image, video, thumbnail, place or faces.","inputSchema":{"type":"object","properties":{"id":id},"required":["id"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"photos_refresh","description":"Read the user's Apple iCloud Photos metadata (personal library) from Apple into the local cache, a bounded batch at a time; a large library takes several calls. Never downloads pictures, never changes photos or albums at Apple and sends no queued changes. Never opens a login prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}}));
    let page = json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":drive::MAX_LIMIT,"default":25}});
    let cloud_path = json!({"type":"string","minLength":1,"maxLength":drive::MAX_PATH_BYTES,"description":format!("Cloud path in Apple iCloud Drive, such as /Documents/Taxes. / is the top. Names are matched as Apple lists them; ., .., empty names and control characters are refused. At most {} names.", drive::MAX_DEPTH)});
    let id = json!({"type":"string","minLength":1,"maxLength":drive::MAX_ID_BYTES,"description":"Item id from drive_list, drive_get or drive_search. At most 512 UTF-8 bytes."});
    let mut folder = page.clone();
    folder["path"] = cloud_path.clone();
    folder["id"] = id.clone();
    tools.push(json!({"name":"drive_list","description":"List one Apple iCloud Drive folder from the local cache: names, kinds, sizes and dates, folders first. Give path or id; the top folder by default. Says whether drive_refresh has read the folder. No file contents. Read-only.","inputSchema":{"type":"object","properties":folder,"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"drive_get","description":"Read one cached Apple iCloud Drive item's details by path or id: name, kind, size, dates, parent and cloud path. No file contents. Read-only.","inputSchema":{"type":"object","properties":{"path":cloud_path.clone(),"id":id.clone()},"additionalProperties":false},"annotations":read}));
    let mut search = page.clone();
    search["query"] = json!({"type":"string","minLength":1,"maxLength":drive::MAX_QUERY_BYTES,"description":"Words matched against item names, ignoring case and accents. At most 256 UTF-8 bytes."});
    tools.push(json!({"name":"drive_search","description":"Search the names of cached Apple iCloud Drive items in folders drive_refresh has read. At most 1000 matches, most recently changed first. Read-only.","inputSchema":{"type":"object","properties":search,"required":["query"],"additionalProperties":false},"annotations":read}));
    tools.push(json!({"name":"drive_refresh","description":"Read one Apple iCloud Drive folder's item list from Apple into the local cache, with each folder on its path. Give path or a cached folder id; the top folder by default. Reads names, kinds, sizes and dates only: never file contents, and never uploads, renames, moves or deletes. Sends no queued changes. Never opens a login prompt.","inputSchema":{"type":"object","properties":{"path":cloud_path,"id":id},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}}));
    tools.push(drive_write::tool());
    tools.push(photos_write::tool());
    tools.push(calendar_write::tool());
    tools.push(contacts_write::tool());
    tools.push(json!({"name":"auth_status","description":"Apple iCloud sign-in status and setup instructions. Never accepts secrets or opens a prompt.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    tools
}
