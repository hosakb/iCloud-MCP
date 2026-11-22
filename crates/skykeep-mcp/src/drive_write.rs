//! `drive_write`: changes to Apple iCloud Drive, one action per call: `create_folder`,
//! `rename`, `move`, `trash`, `delete_forever`, `upload` and `download`.
//!
//! Drive has no durable outbox here, so an action runs as soon as it is allowed and the reply
//! says what happened: done (with what was checked), failed, or unknown when the connection
//! ended before Apple's answer was read. Nothing is retried on its own. Before a change, the
//! folder is read from Apple: a new folder or upload whose name is already there is refused
//! (`name_taken`), a rename Apple already shows answers `already_has_this_name`, and a move
//! already made answers `already_in_destination`. Apple's listing can lag behind a file it
//! has just filed. An upload retried in that moment is filed by Apple under a free name such
//! as "plan 2.txt", never over the first, and its reply says `renamed_by_apple: true`.
//!
//! The engine (`quince_icloud::drive`) makes the requests. Targets are found in the local
//! cache, by cloud path or id as the read tools find them, and each action holds to the
//! version cached: an item Apple changed since is refused, not written over.
//!
//! Who approves. [`is_destructive`] names the deletions in one place. They, and a rename or
//! move of a folder that holds many items (or an unknown number), always need the person's
//! approval, with a preview of what they affect. The person approves or denies them either
//! in the MCP client's approval form, under the `reads_and_writes` and `writes_only` modes
//! (where every Drive change is asked), or with `skykeep-mcp approve` in their own terminal,
//! under `yolo` or with no approval mode. No mode lets them run without one of the two. Upload and download touch only the exchange folder ([`crate::exchange`]).
use crate::account::{binding, Binding};
use crate::drive::{self, Stop, Target, PAUSED};
use crate::exchange::{self, hides_text};
use quince_auth::{Client, Endpoints};
use quince_core::drive::{DriveItem, ROOT};
use quince_core::Library;
use quince_icloud::drive::TRASH;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const TOOL: &str = "drive_write";
/// The actions, as the tool's schema names them.
pub const ACTIONS: [&str; 7] = [
    "create_folder",
    "rename",
    "move",
    "trash",
    "delete_forever",
    "upload",
    "download",
];

/// The Drive actions that remove something. The one place they are named.
pub fn is_destructive(action: &str) -> bool {
    matches!(action, "trash" | "delete_forever")
}

/// A folder with at least this many items (or an unknown number) is renamed or moved only
/// with a preview the person approves.
pub const MANY_ITEMS: i64 = 20;
/// Cached items counted beneath a folder for its preview.
const MAX_COUNTED: usize = 10_000;
/// Names shown in a preview, each cut to `SAMPLE_CHARS`.
const SAMPLE: usize = 20;
const SAMPLE_CHARS: usize = 64;
const SHOWN_CHARS: usize = 255;

pub const UNKNOWN: &str = "drive_write_outcome_unknown_run_drive_refresh_before_retrying";
/// delete_forever moved the item to Recently Deleted, and Apple is known not to have deleted
/// it from there.
pub const IN_RECENTLY_DELETED: &str = "drive_trashed_not_deleted_forever_it_is_in_recently_deleted";
/// The same, and the deletion from Recently Deleted needs a fresh sign-in.
pub const IN_RECENTLY_DELETED_LOGIN: &str = "drive_trashed_not_deleted_forever_login_required";
const NOT_CONFIRMED: &str = "drive_write_not_confirmed_run_drive_refresh";
const CHANGED: &str = "drive_item_changed_run_drive_refresh";
const NO_EXCHANGE: &str = "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal";

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DriveWrite {
    CreateFolder {
        path: Option<String>,
        id: Option<String>,
        name: String,
    },
    Rename {
        path: Option<String>,
        id: Option<String>,
        name: String,
    },
    Move {
        path: Option<String>,
        id: Option<String>,
        destination_path: Option<String>,
        destination_id: Option<String>,
    },
    Trash {
        path: Option<String>,
        id: Option<String>,
    },
    DeleteForever {
        path: Option<String>,
        id: Option<String>,
    },
    Upload {
        path: Option<String>,
        id: Option<String>,
        local: String,
        name: Option<String>,
    },
    Download {
        path: Option<String>,
        id: Option<String>,
        local: Option<String>,
    },
}

impl DriveWrite {
    pub(crate) fn action(&self) -> &'static str {
        match self {
            DriveWrite::CreateFolder { .. } => "create_folder",
            DriveWrite::Rename { .. } => "rename",
            DriveWrite::Move { .. } => "move",
            DriveWrite::Trash { .. } => "trash",
            DriveWrite::DeleteForever { .. } => "delete_forever",
            DriveWrite::Upload { .. } => "upload",
            DriveWrite::Download { .. } => "download",
        }
    }

    fn located(&self) -> (Option<&str>, Option<&str>) {
        match self {
            DriveWrite::CreateFolder { path, id, .. }
            | DriveWrite::Rename { path, id, .. }
            | DriveWrite::Move { path, id, .. }
            | DriveWrite::Trash { path, id }
            | DriveWrite::DeleteForever { path, id }
            | DriveWrite::Upload { path, id, .. }
            | DriveWrite::Download { path, id, .. } => (path.as_deref(), id.as_deref()),
        }
    }
}

/// An item to change: exactly one of path and id, never the top folder or Recently Deleted.
fn item_target(path: Option<&str>, id: Option<&str>) -> Result<Target, &'static str> {
    match (path, id) {
        (Some(path), None) => {
            let names = drive::path_names(path)?;
            if names.is_empty() {
                return Err("top_folder_cannot_be_changed");
            }
            Ok(Target::Path(names))
        }
        (None, Some(id)) => {
            drive::checked_id(id)?;
            if id.contains(TRASH) {
                return Err("trash_not_supported");
            }
            if id == ROOT {
                return Err("top_folder_cannot_be_changed");
            }
            Ok(Target::Id(id.to_string()))
        }
        _ => Err("invalid_arguments"),
    }
}

/// A folder to put something in: exactly one of path and id (`/` is the top folder).
fn folder_target(path: Option<&str>, id: Option<&str>) -> Result<Target, &'static str> {
    if path.is_none() && id.is_none() {
        return Err("invalid_arguments");
    }
    drive::folder_target(path, id)
}

/// A name for iCloud Drive: one cloud path name, no hiding characters, nothing the engine
/// would trim, and none Apple refuses (a leading dot, a colon).
fn cloud_name(name: &str) -> Result<(), &'static str> {
    if !drive::addressable(name) || name.chars().any(hides_text) || name.trim() != name {
        return Err("invalid_name");
    }
    quince_icloud::drive::unusable_name(name).map_err(|_| "invalid_name")
}

/// Checks the arguments before any approval prompt, cache or network use.
pub(crate) fn validate(args: &Value) -> Result<DriveWrite, &'static str> {
    let op: DriveWrite = serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    let (path, id) = op.located();
    match &op {
        DriveWrite::CreateFolder { name, .. } => {
            folder_target(path, id)?;
            cloud_name(name)?;
        }
        DriveWrite::Rename { name, .. } => {
            item_target(path, id)?;
            cloud_name(name)?;
        }
        DriveWrite::Move {
            destination_path,
            destination_id,
            ..
        } => {
            item_target(path, id)?;
            folder_target(destination_path.as_deref(), destination_id.as_deref())?;
        }
        DriveWrite::Trash { .. } | DriveWrite::DeleteForever { .. } => {
            item_target(path, id)?;
        }
        DriveWrite::Upload { local, name, .. } => {
            folder_target(path, id)?;
            let names = exchange::local_names(local)?;
            cloud_name(
                name.as_deref()
                    .unwrap_or(names.last().ok_or("invalid_local_path")?),
            )?;
        }
        DriveWrite::Download { local, .. } => {
            item_target(path, id)?;
            if let Some(local) = local {
                exchange::local_names(local)?;
            }
        }
    }
    Ok(op)
}

// --- The cache.

/// What an action works on, found in the cache.
struct Plan {
    /// The item changed or downloaded.
    item: Option<DriveItem>,
    /// The folder something goes into: a new folder's or upload's parent, a move's destination.
    folder: Option<(String, Option<DriveItem>)>,
}

fn cached_item(library: &Library, target: &Target) -> Result<DriveItem, &'static str> {
    match target {
        Target::Path(names) => {
            drive::resolve_cached(library, names)?.ok_or("top_folder_cannot_be_changed")
        }
        Target::Id(id) => drive::cached_item(library, id)?.ok_or("not_found"),
    }
}

fn cached_folder(
    library: &Library,
    target: &Target,
    missing: &'static str,
) -> Result<(String, Option<DriveItem>), &'static str> {
    let (id, _) = drive::cached_folder(library, target).map_err(|e| match e {
        "not_found" => missing,
        other => other,
    })?;
    let item = if id == ROOT {
        None
    } else {
        library.drive_item(&id).map_err(|_| "library_unavailable")?
    };
    Ok((id, item))
}

fn plan(library: &Library, op: &DriveWrite) -> Result<Plan, &'static str> {
    let (path, id) = op.located();
    let changed = |library: &Library| -> Result<DriveItem, &'static str> {
        let item = cached_item(library, &item_target(path, id)?)?;
        if item.kind == "APP_LIBRARY" {
            return Err("app_library_cannot_be_changed");
        }
        Ok(item)
    };
    Ok(match op {
        DriveWrite::CreateFolder { .. } | DriveWrite::Upload { .. } => Plan {
            item: None,
            folder: Some(cached_folder(
                library,
                &folder_target(path, id)?,
                "parent_not_found",
            )?),
        },
        DriveWrite::Rename { .. } | DriveWrite::Trash { .. } | DriveWrite::DeleteForever { .. } => {
            Plan {
                item: Some(changed(library)?),
                folder: None,
            }
        }
        DriveWrite::Download { .. } => {
            let item = cached_item(library, &item_target(path, id)?)?;
            if item.is_container() {
                return Err("not_a_file");
            }
            Plan {
                item: Some(item),
                folder: None,
            }
        }
        DriveWrite::Move {
            destination_path,
            destination_id,
            ..
        } => {
            let item = changed(library)?;
            let destination = cached_folder(
                library,
                &folder_target(destination_path.as_deref(), destination_id.as_deref())?,
                "destination_not_found",
            )?;
            if item.parent_id.as_deref().unwrap_or(ROOT) == destination.0 {
                return Err("already_in_destination");
            }
            // Never into itself or anything beneath it.
            let mut at = Some(destination.0.clone());
            for _ in 0..=drive::MAX_DEPTH {
                let Some(id) = at else { break };
                if id == item.drivewsid {
                    return Err("cannot_move_a_folder_into_itself");
                }
                at = library
                    .drive_item(&id)
                    .map_err(|_| "library_unavailable")?
                    .and_then(|i| i.parent_id);
            }
            Plan {
                item: Some(item),
                folder: Some(destination),
            }
        }
    })
}

/// Text from Apple as it may be shown: control and hiding characters made visible as
/// U+FFFD, at most `chars` characters.
pub(crate) fn shown(text: &str, chars: usize) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || hides_text(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .take(chars)
        .collect()
}

fn show_item(library: &Library, item: &DriveItem) -> Value {
    json!({
        "id": item.drivewsid,
        "name": shown(&item.display_name(), SHOWN_CHARS),
        "kind": drive::kind(item),
        "size": item.size,
        "etag": item.etag,
        "parent_id": item.parent_id,
        "path": drive::cloud_path(library, item).map(|p| shown(&p, 1024)),
    })
}

fn show_folder(library: &Library, folder: &(String, Option<DriveItem>)) -> Value {
    match &folder.1 {
        Some(item) => show_item(library, item),
        None => json!({"id": folder.0, "name": "/", "kind": "folder", "path": "/"}),
    }
}

/// What a change to `folder` reaches, as far as the cache knows: Apple's own count of its
/// items, the items cached beneath it at any depth, folders beneath not yet read, and the
/// first names.
fn affects(library: &Library, folder: &DriveItem) -> Result<Value, &'static str> {
    let listed = drive::listed_map(library);
    let direct = drive::children(library, &folder.drivewsid)?;
    let sample: Vec<String> = direct
        .iter()
        .take(SAMPLE)
        .map(|i| shown(&i.display_name(), SAMPLE_CHARS))
        .collect();
    let mut queue: Vec<DriveItem> = direct;
    let mut counted = 0usize;
    let mut unread = 0usize;
    let mut capped = false;
    while let Some(item) = queue.pop() {
        if counted >= MAX_COUNTED {
            capped = true;
            break;
        }
        counted += 1;
        if item.is_container() {
            if listed.contains_key(&item.drivewsid) {
                queue.extend(drive::children(library, &item.drivewsid)?);
            } else {
                unread += 1;
            }
        }
    }
    Ok(json!({
        "apple_item_count": folder.child_count,
        "folder_read": listed.contains_key(&folder.drivewsid),
        "cached_items_beneath": counted,
        "cached_count_capped": capped,
        "folders_beneath_not_read": unread,
        "first_names": sample,
    }))
}

fn many(library: &Library, item: &DriveItem) -> Result<bool, &'static str> {
    if !item.is_container() {
        return Ok(false);
    }
    if item.child_count.is_none_or(|n| n >= MANY_ITEMS) {
        return Ok(true);
    }
    let reach = affects(library, item)?;
    // What was never read is unknown, and unknown counts as many: a folder not read that
    // holds anything, or one with a folder beneath not read.
    let unread_items = reach["folder_read"] == false && item.child_count != Some(0);
    let unread_below = reach["folders_beneath_not_read"]
        .as_u64()
        .is_some_and(|n| n > 0);
    Ok(unread_items
        || unread_below
        || reach["cached_items_beneath"]
            .as_u64()
            .is_some_and(|n| n >= MANY_ITEMS as u64))
}

/// Whether this action needs the person's approval whatever the approval mode: a deletion, or
/// a rename or move of a folder that holds many items.
pub(crate) fn needs_person(library: &Library, op: &DriveWrite) -> Result<bool, &'static str> {
    if is_destructive(op.action()) {
        return Ok(true);
    }
    if matches!(op, DriveWrite::Rename { .. } | DriveWrite::Move { .. }) {
        if let Some(item) = plan(library, op)?.item {
            return many(library, &item);
        }
    }
    Ok(false)
}

/// Whether the approval is asked in the terminal: when the approval mode has no form for it
/// (no mode set, or `yolo`). The other modes ask every Drive change in the form.
pub(crate) fn goes_to_terminal(
    library: &Library,
    mode: Option<&str>,
    op: &DriveWrite,
) -> Result<bool, &'static str> {
    Ok(mode.is_none_or(|m| m == "yolo") && needs_person(library, op)?)
}

/// What the approval shows and is bound to: the action, and what it reaches as cached. The
/// same arguments over the same cache give the same preview, so a change in between makes an
/// approval stale.
pub(crate) fn preview(library: &Library, args: &Value) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let plan = plan(library, &op)?;
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
    });
    if let Some(item) = &plan.item {
        value["item"] = show_item(library, item);
        if item.is_container() {
            value["affects"] = affects(library, item)?;
        }
    }
    if let Some(folder) = &plan.folder {
        value[if matches!(op, DriveWrite::Move { .. }) {
            "destination"
        } else {
            "into"
        }] = show_folder(library, folder);
    }
    match &op {
        DriveWrite::Trash { .. } => {
            value["effect"] = json!("moved to Recently Deleted, where Apple keeps it about 30 days")
        }
        DriveWrite::DeleteForever { .. } => {
            value["effect"] =
                json!("moved to Recently Deleted and then deleted for good; it cannot be recovered")
        }
        DriveWrite::Upload { local, name, .. } => {
            value["local_file"] = json!(local);
            value["name_in_drive"] = json!(name
                .clone()
                .or_else(|| local.rsplit('/').next().map(str::to_string)));
            value["effect"] = json!("reads one file from the exchange folder and adds it; an existing name is refused, never replaced");
        }
        DriveWrite::Download { local, .. } => {
            // Without `local`, the name the download takes: the item's, cleaned.
            value["local_file"] = json!(local.clone().or_else(|| plan
                .item
                .as_ref()
                .and_then(|i| exchange::local_name_from_apple(&i.display_name()))));
            value["effect"] = json!(
                "writes one new file into the exchange folder; an existing file is never replaced"
            );
        }
        DriveWrite::Rename { name, .. } | DriveWrite::CreateFolder { name, .. } => {
            value["new_name"] = json!(name);
        }
        DriveWrite::Move { .. } => {}
    }
    Ok(value)
}

/// A preview in plain lines, for the terminal approval: what the change does, the item and
/// its path, where it goes, and what it reaches.
pub(crate) fn plain_preview(preview: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let item = |v: &Value| match v["path"].as_str() {
        Some(path) => format!("{} ({path})", text(&v["name"])),
        None => format!("{} (id {})", text(&v["name"]), text(&v["id"])),
    };
    let mut lines = vec![format!(
        "Drive change: {}{}",
        text(&preview["action"]),
        if preview["destructive"] == true {
            ", a deletion"
        } else {
            ""
        }
    )];
    if preview["effect"].is_string() {
        lines.push(format!("What it does: {}", text(&preview["effect"])));
    }
    if preview["item"].is_object() {
        lines.push(format!("Item: {}", item(&preview["item"])));
    }
    if preview["new_name"].is_string() {
        lines.push(format!("New name: {}", text(&preview["new_name"])));
    }
    if preview["destination"].is_object() {
        lines.push(format!("Move into: {}", item(&preview["destination"])));
    }
    if preview["into"].is_object() {
        lines.push(format!("Into: {}", item(&preview["into"])));
    }
    if preview["local_file"].is_string() {
        lines.push(format!(
            "Local file in the exchange folder: {}",
            text(&preview["local_file"])
        ));
    }
    if preview["name_in_drive"].is_string() {
        lines.push(format!(
            "Name in iCloud Drive: {}",
            text(&preview["name_in_drive"])
        ));
    }
    let affects = &preview["affects"];
    if affects.is_object() {
        let count = affects["apple_item_count"]
            .as_i64()
            .map_or("an unknown number of".to_string(), |n| n.to_string());
        lines.push(format!(
            "It reaches: {count} items directly, as Apple counts them; {} items cached beneath it{}; {} folders beneath not read yet{}.",
            affects["cached_items_beneath"].as_u64().unwrap_or(0),
            if affects["cached_count_capped"] == true { " (counting stopped)" } else { "" },
            affects["folders_beneath_not_read"].as_u64().unwrap_or(0),
            if affects["folder_read"] == true { "" } else { "; the folder itself was never read, so what it holds is unknown" },
        ));
        if let Some(names) = affects["first_names"].as_array().filter(|n| !n.is_empty()) {
            let names: Vec<String> = names.iter().map(text).collect();
            lines.push(format!("First names: {}", names.join(", ")));
        }
    }
    lines.join("\n")
}

// --- Running an action.

/// Time limits of one action.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct WriteLimits {
    /// create_folder, rename, move, trash and delete_forever.
    pub change_timeout: Duration,
    /// upload and download; `None` scales with the size limit ([`transfer_time`]).
    pub transfer_timeout: Option<Duration>,
    /// The folder reads made before a change.
    pub read: drive::RefreshLimits,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            change_timeout: Duration::from_secs(60),
            transfer_timeout: None,
            read: drive::RefreshLimits::default(),
        }
    }
}

/// How long one upload or download may take under a size limit of `max_bytes`: two minutes,
/// and one second more per 256 KB of the limit (about nine minutes at 100 MB).
pub fn transfer_time(max_bytes: u64) -> Duration {
    Duration::from_secs(120 + max_bytes / (256 * 1024))
}

/// Makes the session's client for a private directory.
pub type MakeClient = Arc<dyn Fn(&Path) -> Result<Client, &'static str> + Send + Sync>;

/// Test seam: synthetic endpoints and short limits. Production uses neither.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct Seam {
    pub client: Option<MakeClient>,
    pub limits: WriteLimits,
}

fn client_for(dir: &Path, seam: &Seam) -> Result<Client, &'static str> {
    if let Some(make) = &seam.client {
        return make(dir);
    }
    let store = crate::storage::open_store(dir)?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")
}

struct Conn {
    client: Client,
    dsid: String,
    drivews: String,
    docws: Option<String>,
}

async fn connect(dir: &Path, seam: &Seam) -> Result<Conn, &'static str> {
    let mut client = client_for(dir, seam)?;
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
    let dsid = client.session().dsid.clone().ok_or("login_required")?;
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if binding(&library, &dsid)? != Binding::Same {
        return Err("account_mismatch_use_new_state_directory");
    }
    if client.data_access_missing().contains(&"drive") {
        return Err(PAUSED);
    }
    let drivews = client
        .session()
        .service_url("drivews")
        .ok_or("drive_service_unavailable")?;
    let docws = client.session().service_url("docws");
    Ok(Conn {
        client,
        dsid,
        drivews,
        docws,
    })
}

fn apple(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::ReauthRequired => "login_required",
        E::NeedsDeviceApproval => PAUSED,
        E::Network(_) | E::Busy(_) => UNKNOWN,
        E::Refused(_) if quince_icloud::drive::transient(e) => UNKNOWN,
        E::Conflict => CHANGED,
        _ => NOT_CONFIRMED,
    }
}

fn read_stop(stop: Stop, gone: &'static str) -> &'static str {
    match stop {
        Stop::FolderGone | Stop::InTrash => gone,
        Stop::Reauth => "login_required",
        Stop::Paused => PAUSED,
        _ => "drive_read_failed_nothing_sent",
    }
}

/// A folder's items as Apple has them now, under the read limits.
async fn fresh(
    conn: &Conn,
    id: &str,
    limits: &WriteLimits,
    gone: &'static str,
) -> Result<Vec<DriveItem>, &'static str> {
    let mut budget = limits.read.max_total_bytes;
    drive::folder_read(&conn.client, &conn.drivews, id, &limits.read, &mut budget)
        .await
        .map_err(|stop| read_stop(stop, gone))
}

/// The item of `items` other than `except` whose name is `name`, as Drive compares names:
/// composed and without regard to case.
fn taken<'a>(items: &'a [DriveItem], name: &str, except: Option<&str>) -> Option<&'a DriveItem> {
    let key = quince_core::fold::name_key(name);
    items.iter().find(|i| {
        Some(i.drivewsid.as_str()) != except
            && quince_core::fold::name_key(&i.display_name()) == key
    })
}

/// The item as Apple lists it in its folder now; refused when it is gone or changed since it
/// was cached.
async fn current(
    conn: &Conn,
    item: &DriveItem,
    limits: &WriteLimits,
) -> Result<Vec<DriveItem>, &'static str> {
    let parent = item.parent_id.as_deref().unwrap_or(ROOT);
    let items = fresh(conn, parent, limits, "not_found_at_apple_run_drive_refresh").await?;
    let now = items
        .iter()
        .find(|i| i.drivewsid == item.drivewsid)
        .ok_or("not_found_at_apple_run_drive_refresh")?;
    if item.etag.is_some() && now.etag != item.etag {
        return Err(CHANGED);
    }
    Ok(items)
}

/// What a finished (or interrupted) action leaves for the cache: folders whose cached listing
/// is no longer current, and items gone from Drive.
#[derive(Default)]
struct Effects {
    unlist: Vec<String>,
    forget: Vec<String>,
}

/// Marks the folders as not read (so reads say to refresh them) and forgets the items, only
/// while the directory still belongs to the account the action ran as.
fn settle(dir: &Path, dsid: &str, effects: &Effects) {
    let Ok(mut library) = Library::open(dir) else {
        return;
    };
    if binding(&library, dsid) != Ok(Binding::Same) {
        return;
    }
    for id in &effects.forget {
        let _ = library.forget_drive_folder(id);
    }
    let mut listed = drive::listed_map(&library);
    for id in effects.unlist.iter().chain(&effects.forget) {
        listed.remove(id);
    }
    // Folders forgotten with a trashed folder are no longer read, as after a refresh.
    listed.retain(|id, _| id == ROOT || library.drive_item(id).ok().flatten().is_some());
    let _ = library.set_state(drive::LISTED_KEY, &Value::Object(listed).to_string());
}

/// What the local side of a transfer was given before anything is asked of Apple.
enum Local {
    None,
    Upload(exchange::Staged, String, u64),
    Download(exchange::Incoming, String, u64),
}

/// Runs one action. `approved` says a person approved this exact action (in the form or the
/// terminal). Caller holds the private directory's lock.
pub(crate) async fn run(
    dir: &Path,
    seam: &Seam,
    op: DriveWrite,
    approved: bool,
) -> Result<Value, &'static str> {
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if library
        .state(quince_sync::queue::ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .is_none()
    {
        return Err("login_required");
    }
    if !approved {
        let mode = crate::policy(&library, dir)?.map(|(p, _)| p.mode);
        if mode.as_deref().is_some_and(|m| m != "yolo") || needs_person(&library, &op)? {
            return Err("approval_required");
        }
    }
    let plan = plan(&library, &op)?;
    let shown_before = match &plan.item {
        Some(item) if item.is_container() => {
            if drive::listed_map(&library).contains_key(&item.drivewsid) {
                drive::children(&library, &item.drivewsid)?
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    // The local side first: nothing is asked of Apple for a file that cannot be read or a
    // name that is taken.
    let local = match &op {
        DriveWrite::Upload { local, name, .. } => {
            let exchange = exchange::configured(&library)?.ok_or(NO_EXCHANGE)?;
            let (file, size) = exchange::open_upload(&exchange, local)?;
            let name = name
                .clone()
                .or_else(|| local.rsplit('/').next().map(str::to_string))
                .ok_or("invalid_local_path")?;
            let staged = exchange::stage(dir, file, size, exchange.max_bytes, &name)?;
            Local::Upload(staged, name, exchange.max_bytes)
        }
        DriveWrite::Download { local, .. } => {
            let exchange = exchange::configured(&library)?.ok_or(NO_EXCHANGE)?;
            let item = plan.item.as_ref().ok_or("not_found")?;
            if item
                .size
                .is_some_and(|n| u64::try_from(n).map_or(true, |n| n > exchange.max_bytes))
            {
                return Err("drive_file_over_exchange_limit");
            }
            let names = match local {
                Some(local) => exchange::local_names(local)?,
                None => vec![exchange::local_name_from_apple(&item.display_name())
                    .ok_or("drive_name_not_usable_locally_give_local")?],
            };
            let incoming = exchange::incoming(&exchange, &names)?;
            Local::Download(incoming, names.join("/"), exchange.max_bytes)
        }
        _ => Local::None,
    };
    drop(library);
    let conn = connect(dir, seam).await?;
    let mut effects = Effects::default();
    let limits = seam.limits;
    let timeout = match &local {
        Local::None => limits.change_timeout,
        Local::Upload(_, _, max) | Local::Download(_, _, max) => limits
            .transfer_timeout
            .unwrap_or_else(|| transfer_time(*max)),
    };
    let outcome = tokio::time::timeout(
        timeout,
        act(
            &conn,
            &op,
            &plan,
            &shown_before,
            local,
            &limits,
            &mut effects,
        ),
    )
    .await;
    let result = match outcome {
        Ok(result) => result,
        Err(_) if matches!(op, DriveWrite::Download { .. }) => {
            Err("drive_download_timed_out_nothing_kept")
        }
        Err(_) => Err(UNKNOWN),
    };
    settle(dir, &conn.dsid, &effects);
    result.map(|mut value| {
        value["action"] = json!(op.action());
        if !effects.unlist.is_empty() || !effects.forget.is_empty() {
            value["cache"] = json!(
                "the folders this changed are marked as not read; drive_refresh reads them again"
            );
        }
        value
    })
}

async fn act(
    conn: &Conn,
    op: &DriveWrite,
    plan: &Plan,
    shown_before: &[DriveItem],
    local: Local,
    limits: &WriteLimits,
    effects: &mut Effects,
) -> Result<Value, &'static str> {
    let http = conn.client.http();
    let params = || conn.client.service_params();
    let root = conn.drivews.as_str();
    let parent_of = |item: &DriveItem| item.parent_id.clone().unwrap_or_else(|| ROOT.to_string());
    match op {
        DriveWrite::CreateFolder { name, .. } => {
            let (folder, _) = plan.folder.as_ref().ok_or("parent_not_found")?;
            let items = fresh(conn, folder, limits, "parent_not_found").await?;
            if taken(&items, name, None).is_some() {
                return Err("name_taken");
            }
            effects.unlist.push(folder.clone());
            let made = quince_icloud::drive::create_folder(http, root, params(), folder, name)
                .await
                .map_err(|e| apple(&e))?;
            Ok(json!({
                "outcome": "done",
                "folder": {"id": made.drivewsid, "name": shown(&made.display_name(), SHOWN_CHARS)},
                "checked": "Apple's answer described the new folder; it was not read back",
                "remote_verified": false,
            }))
        }
        DriveWrite::Rename { name, .. } => {
            let item = plan.item.as_ref().ok_or("not_found")?;
            let parent = parent_of(item);
            let items = fresh(
                conn,
                &parent,
                limits,
                "not_found_at_apple_run_drive_refresh",
            )
            .await?;
            let now = items
                .iter()
                .find(|i| i.drivewsid == item.drivewsid)
                .ok_or("not_found_at_apple_run_drive_refresh")?;
            // Asked again after it worked: Apple already shows the name (its version moved
            // on with the rename, so this is looked at first).
            if quince_core::fold::composed(&now.display_name()) == quince_core::fold::composed(name)
            {
                return Err("already_has_this_name");
            }
            if item.etag.is_some() && now.etag != item.etag {
                return Err(CHANGED);
            }
            if taken(&items, name, Some(&item.drivewsid)).is_some() {
                return Err("name_taken");
            }
            effects.unlist.push(parent_of(item));
            quince_icloud::drive::rename(http, root, params(), item, name)
                .await
                .map_err(|e| apple(&e))?;
            Ok(json!({
                "outcome": "done",
                "item": {"id": item.drivewsid, "name": shown(name, SHOWN_CHARS)},
                "checked": "the folder was read back from Apple and shows the new name",
                "remote_verified": true,
            }))
        }
        DriveWrite::Move { .. } => {
            let item = plan.item.as_ref().ok_or("not_found")?;
            let (destination, _) = plan.folder.as_ref().ok_or("destination_not_found")?;
            let there = fresh(conn, destination, limits, "destination_not_found").await?;
            if there.iter().any(|i| i.drivewsid == item.drivewsid) {
                return Err("already_in_destination");
            }
            if taken(&there, &item.display_name(), None).is_some() {
                return Err("name_taken");
            }
            current(conn, item, limits).await?;
            effects.unlist.push(parent_of(item));
            effects.unlist.push(destination.clone());
            quince_icloud::drive::move_to(http, root, params(), item, destination)
                .await
                .map_err(|e| apple(&e))?;
            Ok(json!({
                "outcome": "done",
                "item": {"id": item.drivewsid, "name": shown(&item.display_name(), SHOWN_CHARS)},
                "destination_id": destination,
                "checked": "the destination folder was read back from Apple and holds the item",
                "remote_verified": true,
            }))
        }
        DriveWrite::Trash { .. } | DriveWrite::DeleteForever { .. } => {
            let item = plan.item.as_ref().ok_or("not_found")?;
            current(conn, item, limits).await?;
            effects.unlist.push(parent_of(item));
            let removed = quince_icloud::drive::delete_as_shown(
                http,
                root,
                params(),
                item,
                shown_before,
                item.etag.as_deref(),
            )
            .await
            .map_err(|e| apple(&e))?;
            if removed != quince_icloud::drive::Removed::Done {
                return Err("drive_folder_changed_not_trashed_run_drive_refresh");
            }
            // In Recently Deleted from here on: no longer a live item.
            effects.forget.push(item.drivewsid.clone());
            let shown_item =
                json!({"id": item.drivewsid, "name": shown(&item.display_name(), SHOWN_CHARS)});
            if matches!(op, DriveWrite::Trash { .. }) {
                return Ok(json!({
                    "outcome": "done",
                    "item": shown_item,
                    "checked": "the folder was read back from Apple and no longer holds the item; Apple keeps it in Recently Deleted for about 30 days",
                    "remote_verified": true,
                }));
            }
            quince_icloud::drive::delete_forever(http, root, params(), item)
                .await
                .map_err(|e| delete_forever_error(&e))?;
            Ok(json!({
                "outcome": "done",
                "item": shown_item,
                "checked": "Recently Deleted was read back from Apple and no longer holds the item",
                "remote_verified": true,
            }))
        }
        DriveWrite::Upload { .. } => {
            let Local::Upload(staged, name, _) = local else {
                return Err("invalid_arguments");
            };
            let (folder, _) = plan.folder.as_ref().ok_or("parent_not_found")?;
            let docws = conn.docws.as_deref().ok_or("drive_service_unavailable")?;
            let items = fresh(conn, folder, limits, "parent_not_found").await?;
            if taken(&items, &name, None).is_some() {
                return Err("name_taken");
            }
            effects.unlist.push(folder.clone());
            let progress: quince_icloud::drive::Progress = Arc::new(|_, _| {});
            let filed = quince_icloud::drive::upload(
                http,
                docws,
                root,
                params(),
                folder,
                &staged.path,
                progress,
            )
            .await
            .map_err(|e| apple(&e))?;
            drop(staged);
            let filed_name = filed.display_name();
            Ok(json!({
                "outcome": "done",
                "item": {"id": filed.drivewsid, "name": shown(&filed_name, SHOWN_CHARS), "size": filed.size},
                "renamed_by_apple": quince_core::fold::composed(&filed_name) != quince_core::fold::composed(&name),
                "checked": "Apple's receipt carried the checksum of exactly the bytes sent, and the file was read back from Apple at that size",
                "remote_verified": true,
            }))
        }
        DriveWrite::Download { .. } => {
            let Local::Download(mut incoming, local, max) = local else {
                return Err("invalid_arguments");
            };
            let item = plan.item.as_ref().ok_or("not_found")?;
            let version_checked = item.size != Some(0) && item.etag.is_some();
            let received = if item.size == Some(0) {
                0
            } else {
                let docws = conn.docws.as_deref().ok_or("drive_service_unavailable")?;
                // Exactly the version previewed and approved: refused if Apple has a newer one.
                let link =
                    quince_icloud::drive::download_url_of_version(http, docws, params(), item)
                        .await
                        .map_err(|e| match apple(&e) {
                            "login_required" => "login_required",
                            p if p == PAUSED => PAUSED,
                            CHANGED => CHANGED,
                            _ => "drive_download_failed_nothing_kept",
                        })?;
                fetch_into(&conn.client, &link, &mut incoming, max).await?
            };
            incoming.place()?;
            Ok(json!({
                "outcome": "done",
                "local_file": local,
                "bytes": received,
                "item": {"id": item.drivewsid, "name": shown(&item.display_name(), SHOWN_CHARS)},
                "checked": if version_checked {
                    "every byte Apple said it would send arrived, from a link asked for the cached version, which Apple refuses when it has a newer one; Apple sends no checksum to compare"
                } else if item.size == Some(0) {
                    "the cache lists an empty file, so nothing was asked of Apple and its version was not checked"
                } else {
                    "every byte Apple said it would send arrived; the cache holds no version for the file, so the version was not checked"
                },
                "version_checked": version_checked,
                "remote_verified": false,
            }))
        }
    }
}

/// Why the deletion from Recently Deleted failed, once the item is known to be there. Only an
/// answer that says the item stayed is reported as in Recently Deleted. A busy Drive leaves the
/// outcome unknown. The engine keeps a dropped connection on `deleteItems` as a refusal in its
/// list of failures, so that comes back as unconfirmed (`drive_write_not_confirmed_run_drive_refresh`),
/// as does anything else.
fn delete_forever_error(e: &quince_icloud::Error) -> &'static str {
    match apple(e) {
        "login_required" => IN_RECENTLY_DELETED_LOGIN,
        UNKNOWN => UNKNOWN,
        _ => match e {
            quince_icloud::Error::Refused(why)
                if why.contains("still in Recently Deleted")
                    || why.contains("did not let that be deleted")
                    || why.contains("so it was not deleted") =>
            {
                IN_RECENTLY_DELETED
            }
            _ => NOT_CONFIRMED,
        },
    }
}

/// The bytes at `link` into `incoming`, refused the moment they pass `max`.
async fn fetch_into(
    client: &Client,
    link: &str,
    incoming: &mut exchange::Incoming,
    max: u64,
) -> Result<u64, &'static str> {
    use std::io::Write;
    let mut response = client
        .http()
        .get(link)
        .send()
        .await
        .map_err(|_| "drive_download_failed_nothing_kept")?;
    if !response.status().is_success() {
        return Err("drive_download_failed_nothing_kept");
    }
    let promised = response.content_length();
    if promised.is_some_and(|n| n > max) {
        return Err("drive_file_over_exchange_limit");
    }
    let file = incoming.file.as_mut().ok_or("local_write_failed")?;
    let mut received = 0u64;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() as u64 > max.saturating_sub(received) {
                    return Err("drive_file_over_exchange_limit");
                }
                received += chunk.len() as u64;
                file.write_all(&chunk).map_err(|_| "local_write_failed")?;
            }
            Ok(None) => break,
            Err(_) => return Err("drive_download_incomplete_nothing_kept"),
        }
    }
    if promised.is_some_and(|n| n != received) {
        return Err("drive_download_incomplete_nothing_kept");
    }
    Ok(received)
}

/// The tool's entry in `tools/list`.
pub(crate) fn tool() -> Value {
    let cloud_path = json!({"description":"move only: cloud path of the folder to move the item into, such as /Documents. / is the top folder.","type":"string","minLength":1,"maxLength":drive::MAX_PATH_BYTES});
    let id = json!({"description":"move only: id of the folder to move the item into, from drive_list, drive_get or drive_search, instead of destination_path.","type":"string","minLength":1,"maxLength":drive::MAX_ID_BYTES});
    json!({
        "name": TOOL,
        "description": "Change the user's Apple iCloud Drive: create_folder (path or id of the parent, name), rename (path or id, name), move (path or id, destination_path or destination_id), trash (to Recently Deleted), delete_forever, upload (path or id of the folder, local, optional name) and download (path or id of the file, optional local). Targets are found in the local cache as drive_get finds them, so run drive_refresh on the folder first. local names a file inside the exchange folder the user set up in their terminal: a relative path, never absolute. An existing name is refused, never replaced, in iCloud Drive and locally. Deletions and moves or renames of a folder with many items always need the user's approval, in the approval form or with skykeep-mcp approve in their terminal; every change follows the approval mode. already_has_this_name and already_in_destination mean the change is already done. The reply says done with what was checked, or an error; an unknown outcome means run drive_refresh and look before trying again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "path": {"description": "Cloud path such as /Documents/Taxes: the item, or the folder for create_folder and upload.", "type": "string", "minLength": 1, "maxLength": drive::MAX_PATH_BYTES},
                "id": {"description": "Item id from drive_list, drive_get or drive_search, instead of path.", "type": "string", "minLength": 1, "maxLength": drive::MAX_ID_BYTES},
                "name": {"description": "New name: for create_folder and rename, or the name an upload gets (default: the local file's name).", "type": "string", "minLength": 1, "maxLength": drive::MAX_NAME_BYTES},
                "destination_path": cloud_path,
                "destination_id": id,
                "local": {"description": "A file inside the exchange folder, as a relative path such as report.pdf or in/report.pdf. Required for upload; for download, default: the item's name.", "type": "string", "minLength": 1, "maxLength": exchange::MAX_LOCAL_BYTES},
            },
            "required": ["action"],
            "additionalProperties": false,
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_parses_and_only_deletions_are_destructive() {
        for action in ACTIONS {
            assert_eq!(
                is_destructive(action),
                ["trash", "delete_forever"].contains(&action)
            );
        }
        assert!(validate(&json!({"action":"rename","path":"/a","name":"b"})).is_ok());
        assert_eq!(
            validate(&json!({"action":"rename","path":"/a","name":"b","extra":1})),
            Err("invalid_arguments")
        );
        assert_eq!(
            validate(&json!({"action":"erase","path":"/a"})),
            Err("invalid_arguments")
        );
    }

    #[test]
    fn names_and_targets_are_checked_before_anything_else() {
        for bad in [
            ".hidden",
            "a:b",
            " pad",
            "pad ",
            "a/b",
            "..",
            "a\u{202E}b",
            "a\u{7}",
        ] {
            assert_eq!(
                validate(&json!({"action":"create_folder","path":"/","name":bad})),
                Err("invalid_name"),
                "{bad:?}"
            );
        }
        assert_eq!(
            validate(&json!({"action":"trash","path":"/"})),
            Err("top_folder_cannot_be_changed")
        );
        assert_eq!(
            validate(&json!({"action":"trash","id":"TRASH_ROOT"})),
            Err("trash_not_supported")
        );
        assert_eq!(
            validate(&json!({"action":"create_folder","name":"x"})),
            Err("invalid_arguments")
        );
        assert_eq!(
            validate(&json!({"action":"upload","path":"/","local":"/etc/passwd"})),
            Err("absolute_local_path_refused")
        );
        assert_eq!(
            validate(&json!({"action":"download","path":"/a","local":"../a"})),
            Err("local_path_traversal_refused")
        );
    }
}
