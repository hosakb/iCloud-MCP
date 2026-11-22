//! `photos_write`: changes to the personal Apple iCloud Photos library, one action per call:
//! `upload` (from the exchange folder), `set_favorite`, `set_title_and_caption`, `set_rating`,
//! `set_keywords`, `set_date`, `create_album`, `rename_album` and `trash` (to Recently Deleted).
//!
//! The engine's CloudKit client (`quince_icloud::cloudkit`) and its Photos code
//! (`quince_icloud::photos`, `quince_icloud::photos_upload`) make the requests. A change runs
//! as soon as it is allowed; nothing is queued or retried here. A photo or album is found in
//! the local cache, as the read tools find it, and each change holds to the version cached:
//! the record is looked up at Apple, refused (`photos_item_changed_run_photos_refresh`) when its
//! change tag moved since the cache read it, and then written with `update_record_exact` over
//! exactly that tag, so CloudKit also refuses it when another device wrote in between. Keywords
//! are the one exception: Apple keeps them as one encoded list, which the engine rebuilds from
//! Apple's record at the moment of writing, so the words added and taken away here are merged
//! into it. The reply says what Apple's answer showed; nothing is read back afterwards, so
//! `remote_verified` stays false.
//!
//! Who approves. [`is_destructive`] (the permissions rules' own classification) names `trash`
//! as the only removal. It always needs the person's approval, in the approval form or with
//! `skykeep-mcp approve` in their terminal, and its card never offers "Always allow". Every
//! other action follows the approval mode and the permissions file. Deleting for good, editing
//! pictures, shared albums and changing many photos at once are not offered. An upload reads one
//! file from the exchange folder ([`crate::exchange`]), under its size limit.
use crate::account::{binding, Binding};
use crate::drive_write::{shown, Seam};
use crate::exchange::{self, hides_text};
use quince_auth::{Client, Endpoints};
use quince_core::photos::{Album, Photo};
use quince_core::Library;
use quince_icloud::cloudkit::ZoneClient;
use quince_icloud::photos as apple;
use quince_icloud::photos_upload::{self as upload, Outcome, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const TOOL: &str = "photos_write";
/// The actions, as the tool's schema names them.
pub const ACTIONS: [&str; 9] = [
    "upload",
    "set_favorite",
    "set_title_and_caption",
    "set_rating",
    "set_keywords",
    "set_date",
    "create_album",
    "rename_album",
    "trash",
];

/// The Photos actions that remove something, as the permissions rules decide it for every
/// tool (`permissions::removal_of`): `trash`.
pub fn is_destructive(action: &str) -> bool {
    crate::permissions::is_deletion(TOOL, action)
}

/// The file types iCloud Photos takes, in words, as the engine's uploader lists them
/// (`photos_upload`): jpg, jpeg, heic, heif, png, gif, tif, tiff, webp, mov, mp4, m4v, avi, dng.
pub const UPLOAD_TYPES: &str = "JPEG (.jpg, .jpeg), HEIC (.heic, .heif), PNG, GIF, TIFF (.tif, .tiff), WebP, DNG, and the videos MOV, MP4, M4V and AVI";
pub const MAX_TITLE_CHARS: usize = 255;
pub const MAX_CAPTION_CHARS: usize = 2048;
pub const MAX_ALBUM_NAME_CHARS: usize = 255;
pub const MAX_KEYWORDS: usize = 50;
pub const MAX_KEYWORD_CHARS: usize = 64;
/// The years a new date may fall in.
const YEARS: std::ops::RangeInclusive<i64> = 1900..=2100;
const SHOWN_CHARS: usize = 255;

pub const UNKNOWN: &str = "photos_write_outcome_unknown_run_photos_refresh_before_retrying";
const NOT_CONFIRMED: &str = "photos_write_not_confirmed_run_photos_refresh";
pub const CHANGED: &str = "photos_item_changed_run_photos_refresh";
const GONE: &str = "photos_item_not_found_at_apple_run_photos_refresh";
const NO_VERSION: &str = "photos_item_version_unknown_run_photos_refresh";
const PAUSED: &str = "photos_paused_approve_on_apple_device";
const NO_EXCHANGE: &str = "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal";

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PhotosWrite {
    Upload {
        local: String,
    },
    SetFavorite {
        id: String,
        favorite: bool,
    },
    SetTitleAndCaption {
        id: String,
        title: Option<String>,
        caption: Option<String>,
    },
    SetRating {
        id: String,
        rating: u8,
    },
    SetKeywords {
        id: String,
        keywords: Vec<String>,
    },
    SetDate {
        id: String,
        taken_at: String,
    },
    CreateAlbum {
        name: String,
    },
    RenameAlbum {
        album_id: String,
        name: String,
    },
    Trash {
        id: String,
    },
}

impl PhotosWrite {
    pub(crate) fn action(&self) -> &'static str {
        match self {
            PhotosWrite::Upload { .. } => "upload",
            PhotosWrite::SetFavorite { .. } => "set_favorite",
            PhotosWrite::SetTitleAndCaption { .. } => "set_title_and_caption",
            PhotosWrite::SetRating { .. } => "set_rating",
            PhotosWrite::SetKeywords { .. } => "set_keywords",
            PhotosWrite::SetDate { .. } => "set_date",
            PhotosWrite::CreateAlbum { .. } => "create_album",
            PhotosWrite::RenameAlbum { .. } => "rename_album",
            PhotosWrite::Trash { .. } => "trash",
        }
    }

    /// The photo the action changes, if it changes one.
    fn photo_id(&self) -> Option<&str> {
        match self {
            PhotosWrite::SetFavorite { id, .. }
            | PhotosWrite::SetTitleAndCaption { id, .. }
            | PhotosWrite::SetRating { id, .. }
            | PhotosWrite::SetKeywords { id, .. }
            | PhotosWrite::SetDate { id, .. }
            | PhotosWrite::Trash { id } => Some(id),
            _ => None,
        }
    }
}

/// Text a person typed for Photos: at most `max` characters, nothing that hides or reorders
/// text, and no control characters (a caption may hold line breaks).
fn text_ok(text: &str, max: usize, lines: bool) -> bool {
    text.chars().count() <= max
        && !text
            .chars()
            .any(|c| hides_text(c) || (c.is_control() && !(lines && c == '\n')))
}

fn name_ok(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() || name.trim() != name || !text_ok(name, MAX_ALBUM_NAME_CHARS, false)
    {
        return Err("invalid_album_name");
    }
    Ok(())
}

fn id_ok(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > crate::photos::MAX_ID_BYTES || id.chars().any(char::is_control) {
        return Err("invalid_arguments");
    }
    Ok(())
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM:SS` followed by `Z` or an offset `+HH:MM` / `-HH:MM`, as unix seconds.
pub(crate) fn parse_moment(text: &str) -> Result<i64, &'static str> {
    const BAD: &str = "invalid_date";
    let b = text.as_bytes();
    if !(b.len() == 20 || b.len() == 25) || !text.is_ascii() {
        return Err(BAD);
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64, &'static str> {
        let part = &text[r];
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return Err(BAD);
        }
        part.parse().map_err(|_| BAD)
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return Err(BAD);
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !YEARS.contains(&y) || !(1..=12).contains(&m) || d < 1 || hh > 23 || mm > 59 || ss > 59 {
        return Err(BAD);
    }
    let next = if m == 12 {
        days_from_civil(y + 1, 1, 1)
    } else {
        days_from_civil(y, m + 1, 1)
    };
    if d > next - days_from_civil(y, m, 1) {
        return Err(BAD);
    }
    let offset = match b[19] {
        b'Z' if b.len() == 20 => 0,
        b'+' | b'-' if b.len() == 25 && b[22] == b':' => {
            let (oh, om) = (num(20..22)?, num(23..25)?);
            if oh > 18 || om > 59 {
                return Err(BAD);
            }
            let o = oh * 3600 + om * 60;
            if b[19] == b'-' {
                -o
            } else {
                o
            }
        }
        _ => return Err(BAD),
    };
    Ok(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// Checks the arguments before any approval prompt, cache or network use.
pub(crate) fn validate(args: &Value) -> Result<PhotosWrite, &'static str> {
    let op: PhotosWrite = serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    if let Some(id) = op.photo_id() {
        id_ok(id)?;
    }
    match &op {
        PhotosWrite::Upload { local } => {
            let names = exchange::local_names(local)?;
            let name = names.last().ok_or("invalid_local_path")?;
            if let Some(refused) = upload::refusal(name, upload::MIN_BYTES) {
                return Err(refusal_code(&refused));
            }
        }
        PhotosWrite::SetTitleAndCaption { title, caption, .. } => {
            if title.is_none() && caption.is_none() {
                return Err("title_or_caption_required");
            }
            if title
                .as_deref()
                .is_some_and(|t| !text_ok(t, MAX_TITLE_CHARS, false))
            {
                return Err("invalid_title");
            }
            if caption
                .as_deref()
                .is_some_and(|c| !text_ok(c, MAX_CAPTION_CHARS, true))
            {
                return Err("invalid_caption");
            }
        }
        PhotosWrite::SetRating { rating, .. } => {
            if *rating > 5 {
                return Err("invalid_rating");
            }
        }
        PhotosWrite::SetKeywords { keywords, .. } => {
            if keywords.len() > MAX_KEYWORDS {
                return Err("too_many_keywords");
            }
            for (i, word) in keywords.iter().enumerate() {
                if word.trim().is_empty()
                    || word.trim() != word
                    || !text_ok(word, MAX_KEYWORD_CHARS, false)
                {
                    return Err("invalid_keyword");
                }
                if keywords[..i].iter().any(|w| w.eq_ignore_ascii_case(word)) {
                    return Err("duplicate_keyword");
                }
            }
        }
        PhotosWrite::SetDate { taken_at, .. } => {
            parse_moment(taken_at)?;
        }
        PhotosWrite::CreateAlbum { name } => name_ok(name)?,
        PhotosWrite::RenameAlbum { album_id, name } => {
            id_ok(album_id)?;
            name_ok(name)?;
        }
        PhotosWrite::SetFavorite { .. } | PhotosWrite::Trash { .. } => {}
    }
    Ok(op)
}

fn refusal_code(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::Kind(_) => "photos_file_type_not_supported",
        Refusal::TooSmall => "photos_file_too_small",
        Refusal::TooLarge => "photos_file_too_large",
        Refusal::NameTooLong => "photos_file_name_too_long",
        Refusal::Duplicate => "photos_duplicate_already_in_library",
        Refusal::OverQuota => "icloud_storage_full_nothing_uploaded",
    }
}

/// Whether this action needs the person's approval whatever the approval mode or rules say:
/// a removal (`trash`).
pub(crate) fn needs_person(op: &PhotosWrite) -> bool {
    is_destructive(op.action())
}

// --- The cache.

/// A photo the read tools show: the personal library's, neither hidden nor deleted.
fn cached_photo(library: &Library, id: &str) -> Result<Photo, &'static str> {
    let photo = library
        .photo(id)
        .map_err(|_| "library_unavailable")?
        .filter(|p| p.id == id && !p.hidden && !p.deleted)
        .ok_or("not_found")?;
    if library
        .photo_zone(id)
        .map_err(|_| "library_unavailable")?
        .is_some()
    {
        return Err("not_found");
    }
    Ok(photo)
}

fn cached_album(library: &Library, id: &str) -> Result<Album, &'static str> {
    library
        .albums()
        .map_err(|_| "library_unavailable")?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or("album_not_found")
}

/// A cached album other than `except` that already has `name`, as Photos shows names:
/// composed and without regard to case.
fn album_named(library: &Library, name: &str, except: Option<&str>) -> Result<bool, &'static str> {
    let key = quince_core::fold::name_key(name);
    Ok(library
        .albums()
        .map_err(|_| "library_unavailable")?
        .iter()
        .any(|a| Some(a.id.as_str()) != except && quince_core::fold::name_key(&a.name) == key))
}

fn show_photo(photo: &Photo) -> Value {
    json!({
        "id": photo.id,
        "filename": photo.filename.as_deref().map(|f| shown(f, SHOWN_CHARS)),
        "title": photo.title.as_deref().map(|t| shown(t, SHOWN_CHARS)),
        "caption": photo.caption.as_deref().map(|c| shown(c, SHOWN_CHARS)),
        "taken_at": crate::calendar::utc_text(photo.taken_at),
        "favorite": photo.favorite,
        "rating": photo.rating,
        "keywords": photo.keywords.iter().take(MAX_KEYWORDS).map(|k| shown(k, MAX_KEYWORD_CHARS)).collect::<Vec<_>>(),
        "change_tag": photo.change_tag,
    })
}

fn show_album(album: &Album) -> Value {
    json!({"id": album.id, "name": shown(&album.name, SHOWN_CHARS), "folder": album.folder, "change_tag": album.change_tag})
}

/// What the approval shows and is bound to: the action and what it reaches as cached. The
/// same arguments over the same cache give the same preview, so a change in between makes an
/// approval stale. Nothing local is read for an upload.
pub(crate) fn preview(library: &Library, args: &Value) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
        "library": "personal",
    });
    if let Some(id) = op.photo_id() {
        value["photo"] = show_photo(&cached_photo(library, id)?);
    }
    match &op {
        PhotosWrite::Upload { local } => {
            value["local_file"] = json!(local);
            value["effect"] = json!("reads one file from the exchange folder and adds it to the library as a new photo; nothing in the library is replaced");
        }
        PhotosWrite::SetFavorite { favorite, .. } => value["new_favorite"] = json!(favorite),
        PhotosWrite::SetTitleAndCaption { title, caption, .. } => {
            value["new_title"] = json!(title);
            value["new_caption"] = json!(caption);
            value["effect"] = json!("an empty text clears it; a field left out stays as it is");
        }
        PhotosWrite::SetRating { rating, .. } => value["new_rating"] = json!(rating),
        PhotosWrite::SetKeywords { keywords, .. } => {
            value["new_keywords"] = json!(keywords);
            value["effect"] = json!("words added or taken away compared with the cached list; a word added on another device meanwhile stays");
        }
        PhotosWrite::SetDate { taken_at, .. } => {
            value["new_taken_at"] = json!(crate::calendar::utc_text(parse_moment(taken_at)?));
        }
        PhotosWrite::CreateAlbum { name } => value["new_album_name"] = json!(name),
        PhotosWrite::RenameAlbum { album_id, name } => {
            value["album"] = show_album(&cached_album(library, album_id)?);
            value["new_name"] = json!(name);
        }
        PhotosWrite::Trash { .. } => {
            value["effect"] = json!(
                "moved to Recently Deleted, where Apple keeps it about 30 days; it is not deleted for good"
            );
        }
    }
    Ok(value)
}

/// A preview in plain lines, for the terminal approval: what the change does, the photo (its
/// file name, title and date) or album, and the new values.
pub(crate) fn plain_preview(preview: &Value) -> String {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => "none".to_string(),
        other => other.to_string(),
    };
    let mut lines = vec![format!(
        "Photos change: {}{}",
        text(&preview["action"]),
        if preview["destructive"] == true {
            ", a move to Recently Deleted"
        } else {
            ""
        }
    )];
    if preview["effect"].is_string() {
        lines.push(format!("What it does: {}", text(&preview["effect"])));
    }
    let photo = &preview["photo"];
    if photo.is_object() {
        let mut about = vec![format!("taken {}", text(&photo["taken_at"]))];
        if photo["title"].is_string() {
            about.push(format!("title {}", text(&photo["title"])));
        }
        about.push(format!("id {}", text(&photo["id"])));
        lines.push(format!(
            "Photo: {} ({})",
            photo["filename"].as_str().unwrap_or("no file name"),
            about.join(", ")
        ));
    }
    let album = &preview["album"];
    if album.is_object() {
        lines.push(format!(
            "Album: {} (id {})",
            text(&album["name"]),
            text(&album["id"])
        ));
    }
    if preview["local_file"].is_string() {
        lines.push(format!(
            "Local file in the exchange folder: {}",
            text(&preview["local_file"])
        ));
    }
    for (key, label) in [
        ("new_favorite", "Favorite"),
        ("new_title", "New title"),
        ("new_caption", "New caption"),
        ("new_rating", "New rating"),
        ("new_keywords", "New keywords"),
        ("new_taken_at", "New date"),
        ("new_album_name", "New album"),
        ("new_name", "New name"),
    ] {
        if let Some(value) = preview.get(key).filter(|v| !v.is_null()) {
            let shown = match value {
                Value::Array(words) => words.iter().map(text).collect::<Vec<_>>().join(", "),
                other => text(other),
            };
            lines.push(format!("{label}: {shown}"));
        }
    }
    lines.join("\n")
}

// --- Running an action.

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
    ck: String,
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
    if client.data_access_missing().contains(&apple::SERVICE) {
        return Err(PAUSED);
    }
    let ck = client
        .session()
        .service_url("ckdatabasews")
        .ok_or("photos_service_unavailable")?;
    Ok(Conn { client, dsid, ck })
}

fn engine(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::ReauthRequired => "login_required",
        E::NeedsDeviceApproval => PAUSED,
        E::Network(_) | E::Busy(_) => UNKNOWN,
        E::Conflict => CHANGED,
        _ => NOT_CONFIRMED,
    }
}

/// The record's change tag at Apple now, refused when it is gone or not the version cached.
async fn hold_version(
    zone_client: &ZoneClient<'_>,
    zone: &str,
    record: &str,
    cached: Option<&str>,
) -> Result<String, &'static str> {
    let cached = cached.ok_or(NO_VERSION)?;
    let now = zone_client
        .lookup_record(zone, record)
        .await
        .map_err(|e| match engine(&e) {
            "login_required" => "login_required",
            p if p == PAUSED => PAUSED,
            _ => "photos_read_failed_nothing_sent",
        })?
        .ok_or(GONE)?;
    if now.get("recordChangeTag").and_then(Value::as_str) != Some(cached) {
        return Err(CHANGED);
    }
    Ok(cached.to_string())
}

const ASSET: &str = "CPLAsset";
const ALBUM: &str = "CPLAlbum";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `fields` written over exactly the version cached, checked at Apple first. CloudKit refuses
/// the write when the record moved on in between, and that refusal comes back as
/// [`CHANGED`]: nothing is sent again over a newer version (the engine's `update_record`
/// would retry once with Apple's newest tag).
async fn exact(
    zone_client: &ZoneClient<'_>,
    zone: &str,
    record: &str,
    record_type: &str,
    cached: Option<&str>,
    fields: &Value,
) -> Result<Value, &'static str> {
    let tag = hold_version(zone_client, zone, record, cached).await?;
    zone_client
        .update_record_exact(zone, record, record_type, &tag, fields.clone())
        .await
        .map_err(|e| engine(&e))
}

/// The record in Apple's answer to a change.
fn answered(answer: &Value) -> &Value {
    answer.pointer("/records/0").unwrap_or(&Value::Null)
}

fn new_tag(answer: &Value) -> Option<String> {
    answered(answer)
        .get("recordChangeTag")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether Apple's answer shows `field` with `value`.
fn shows(answer: &Value, field: &str, value: &Value) -> bool {
    answered(answer).pointer(&format!("/fields/{field}/value")) == Some(value)
}

fn base64_text(text: &str) -> String {
    use base64_encode::encode;
    encode(text.as_bytes())
}

/// Base64 as Photos writes its texts, without a dependency of its own.
mod base64_encode {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub(super) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

/// Runs one action. `approved` says a person approved this exact action (in the form or the
/// terminal) or a rule allows it. Caller holds the private directory's lock.
pub(crate) async fn run(
    dir: &Path,
    seam: &Seam,
    op: PhotosWrite,
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
        if mode.as_deref().is_some_and(|m| m != "yolo") || needs_person(&op) {
            return Err("approval_required");
        }
    }
    let photo = match op.photo_id() {
        Some(id) => Some(cached_photo(&library, id)?),
        None => None,
    };
    let album = match &op {
        PhotosWrite::RenameAlbum { album_id, name } => {
            let album = cached_album(&library, album_id)?;
            if quince_core::fold::composed(&album.name) == quince_core::fold::composed(name) {
                return Err("album_already_has_this_name");
            }
            if album_named(&library, name, Some(album_id))? {
                return Err("album_name_taken");
            }
            Some(album)
        }
        PhotosWrite::CreateAlbum { name } => {
            if album_named(&library, name, None)? {
                return Err("album_name_taken");
            }
            None
        }
        _ => None,
    };
    // The local side first: nothing is asked of Apple for a file that cannot be read.
    let staged = match &op {
        PhotosWrite::Upload { local } => {
            let exchange = exchange::configured(&library)?.ok_or(NO_EXCHANGE)?;
            let (file, size) = exchange::open_upload(&exchange, local)?;
            let name = local.rsplit('/').next().ok_or("invalid_local_path")?;
            if let Some(refused) = upload::refusal(name, size) {
                return Err(refusal_code(&refused));
            }
            let modified = file
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            let staged = exchange::stage(dir, file, size, exchange.max_bytes, name)?;
            Some((staged, name.to_string(), modified, exchange.max_bytes))
        }
        _ => None,
    };
    let first_position = library
        .albums()
        .map_err(|_| "library_unavailable")?
        .iter()
        .filter(|a| a.parent.is_none())
        .map(|a| a.position)
        .min();
    drop(library);
    let conn = connect(dir, seam).await?;
    let limits = seam.limits;
    let timeout = match &staged {
        None => limits.change_timeout,
        // Sending, then Apple making the photo: the engine waits up to five minutes on a job
        // that does not move.
        Some((_, _, _, max)) => limits
            .transfer_timeout
            .unwrap_or_else(|| crate::drive_write::transfer_time(*max) + Duration::from_secs(300)),
    };
    let work = Work {
        conn: &conn,
        op: &op,
        photo: photo.as_ref(),
        album: album.as_ref(),
        first_position,
    };
    let outcome = tokio::time::timeout(timeout, work.act(staged)).await;
    let (mut value, settled) = match outcome {
        Ok(result) => result?,
        Err(_) => return Err(UNKNOWN),
    };
    // The cache follows Apple's answer, only while the directory still belongs to the
    // account the change ran as.
    let mut note = "the local cache shows the change";
    if let Ok(mut library) = Library::open(dir) {
        if binding(&library, &conn.dsid) == Ok(Binding::Same) {
            let stored = match settled {
                Settled::Photo(p) => library.put_photos(&[*p]).is_ok(),
                Settled::Album(a) => library.put_album(&a).is_ok(),
                Settled::Nothing => {
                    note = "the new photo is in the local cache after photos_refresh";
                    true
                }
            };
            if !stored {
                note = "the local cache could not be updated; run photos_refresh";
            }
        }
    }
    value["action"] = json!(op.action());
    value["cache"] = json!(note);
    value["remote_verified"] = json!(false);
    Ok(value)
}

/// What the cache takes from a finished change.
enum Settled {
    Photo(Box<Photo>),
    Album(Album),
    Nothing,
}

struct Work<'a> {
    conn: &'a Conn,
    op: &'a PhotosWrite,
    photo: Option<&'a Photo>,
    album: Option<&'a Album>,
    first_position: Option<i64>,
}

type Staged = Option<(exchange::Staged, String, i64, u64)>;

impl Work<'_> {
    async fn act(&self, staged: Staged) -> Result<(Value, Settled), &'static str> {
        let conn = self.conn;
        let http = conn.client.http();
        let params = || conn.client.service_params();
        let zc = ZoneClient::new(http, &conn.ck, apple::CONTAINER, params());
        let zone = apple::library_zone(&zc).await;
        let done = |checked: bool, what: &str| {
            json!({
                "outcome": if checked { "done" } else { "sent_not_confirmed" },
                "checked": if checked {
                    format!("Apple's answer to the change shows {what}; the photo was not read back")
                } else {
                    format!("Apple accepted the change, but its answer did not show {what}; run photos_refresh and look")
                },
            })
        };
        // The photo as it will be cached after the change.
        let mut after = self.photo.cloned();
        let shown_photo = |p: &Photo| json!({"id": p.id, "filename": p.filename.as_deref().map(|f| shown(f, SHOWN_CHARS))});
        match self.op {
            PhotosWrite::Upload { .. } => {
                let (staged, name, modified, _) = staged.ok_or("invalid_arguments")?;
                let up = conn
                    .client
                    .session()
                    .service_url("photosupload")
                    .ok_or("photos_service_unavailable")?;
                let quiet: Arc<dyn Fn(upload::Stage) + Send + Sync> = Arc::new(|_| {});
                let outcome = upload::upload(
                    http,
                    &up,
                    &conn.dsid,
                    &zone,
                    &staged.path,
                    &name,
                    modified,
                    "UTC",
                    0,
                    &uuid::Uuid::new_v4().to_string(),
                    quiet,
                )
                .await
                .map_err(|e| engine(&e))?;
                drop(staged);
                match outcome {
                    Outcome::Refused(refused) => Err(refusal_code(&refused)),
                    Outcome::Uploaded(made) => Ok((
                        json!({
                            "outcome": "done",
                            "photo": {"id": made.asset, "filename": shown(&name, SHOWN_CHARS)},
                            "checked": "Apple's upload job said the photo is made; it was not read back",
                        }),
                        Settled::Nothing,
                    )),
                }
            }
            PhotosWrite::SetFavorite { favorite, .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                let fields = json!({"isFavorite": {"value": i64::from(*favorite)}});
                let answer =
                    exact(&zc, &zone, &p.id, ASSET, p.change_tag.as_deref(), &fields).await?;
                let checked = shows(&answer, "isFavorite", &json!(i64::from(*favorite)));
                p.favorite = *favorite;
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new favorite mark");
                value["photo"] = shown_photo(p);
                value["favorite"] = json!(favorite);
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::SetTitleAndCaption { title, caption, .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                // Apple's names: the title is `captionEnc`, the caption `extendedDescEnc`,
                // each base64 of its UTF-8 bytes; only the fields given are sent.
                let mut fields = serde_json::Map::new();
                if let Some(t) = title {
                    fields.insert("captionEnc".into(), json!({"value": base64_text(t)}));
                }
                if let Some(c) = caption {
                    fields.insert("extendedDescEnc".into(), json!({"value": base64_text(c)}));
                }
                let answer = exact(
                    &zc,
                    &zone,
                    &p.id,
                    ASSET,
                    p.change_tag.as_deref(),
                    &Value::Object(fields),
                )
                .await?;
                let checked = title
                    .as_deref()
                    .is_none_or(|t| shows(&answer, "captionEnc", &json!(base64_text(t))))
                    && caption
                        .as_deref()
                        .is_none_or(|c| shows(&answer, "extendedDescEnc", &json!(base64_text(c))));
                if let Some(t) = title {
                    p.title = (!t.is_empty()).then(|| t.clone());
                }
                if let Some(c) = caption {
                    p.caption = (!c.is_empty()).then(|| c.clone());
                }
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new text");
                value["photo"] = shown_photo(p);
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::SetRating { rating, .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                let fields = json!({"rating": {"value": i64::from(*rating)}});
                let answer =
                    exact(&zc, &zone, &p.id, ASSET, p.change_tag.as_deref(), &fields).await?;
                let checked = shows(&answer, "rating", &json!(i64::from(*rating)));
                p.rating = i64::from(*rating);
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new rating");
                value["photo"] = shown_photo(p);
                value["rating"] = json!(rating);
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::SetKeywords { keywords, .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                // Apple holds keywords as one encoded list, which the engine builds from the
                // record as Apple has it at the moment of writing: the words added and taken
                // away here are merged into that list, so a word another device added after
                // this check stays.
                hold_version(&zc, &zone, &p.id, p.change_tag.as_deref()).await?;
                let (answer, words) = apple::set_keywords(
                    http,
                    &conn.ck,
                    params(),
                    None,
                    &p.id,
                    &p.keywords,
                    keywords,
                )
                .await
                .map_err(|e| engine(&e))?;
                let checked = answered(&answer)
                    .pointer("/fields/keywordsEnc/value")
                    .is_some();
                p.keywords = words.clone();
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new keywords");
                value["photo"] = shown_photo(p);
                value["keywords"] = json!(words
                    .iter()
                    .map(|k| shown(k, MAX_KEYWORD_CHARS))
                    .collect::<Vec<_>>());
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::SetDate { taken_at, .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                let at = parse_moment(taken_at)?;
                let fields = json!({"assetDate": {"value": at * 1000}});
                let answer =
                    exact(&zc, &zone, &p.id, ASSET, p.change_tag.as_deref(), &fields).await?;
                let checked = shows(&answer, "assetDate", &json!(at * 1000));
                p.taken_at = at;
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new date");
                value["photo"] = shown_photo(p);
                value["taken_at"] = json!(crate::calendar::utc_text(at));
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::Trash { .. } => {
                let p = after.as_mut().ok_or("not_found")?;
                // Recently Deleted, as Photos moves a photo there; never `isExpunged`.
                let fields = json!({"isDeleted": {"value": 1}});
                let answer =
                    exact(&zc, &zone, &p.id, ASSET, p.change_tag.as_deref(), &fields).await?;
                let checked = shows(&answer, "isDeleted", &json!(1));
                p.deleted = true;
                p.deleted_at = Some(now_millis() / 1000);
                p.change_tag = new_tag(&answer);
                let mut value = done(checked, "the photo in Recently Deleted");
                value["photo"] = shown_photo(p);
                value["effect"] = json!("in Recently Deleted, where Apple keeps it about 30 days; restore it there in Photos");
                Ok((value, Settled::Photo(Box::new(p.clone()))))
            }
            PhotosWrite::CreateAlbum { name } => {
                let made = apple::create_album(
                    http,
                    &conn.ck,
                    params(),
                    name,
                    self.first_position,
                    None,
                    false,
                )
                .await
                .map_err(|e| engine(&e))?;
                Ok((
                    json!({
                        "outcome": "done",
                        "album": show_album(&made),
                        "checked": "Apple's answer named the new album; it was not read back",
                    }),
                    Settled::Album(made),
                ))
            }
            PhotosWrite::RenameAlbum { name, .. } => {
                let mut album = self.album.cloned().ok_or("album_not_found")?;
                let fields = json!({
                    "albumNameEnc": {"value": base64_text(name)},
                    "userModificationDate": {"value": now_millis()},
                });
                let answer = exact(
                    &zc,
                    &zone,
                    &album.id,
                    ALBUM,
                    album.change_tag.as_deref(),
                    &fields,
                )
                .await?;
                let checked = shows(&answer, "albumNameEnc", &json!(base64_text(name)));
                album.name = name.clone();
                album.change_tag = new_tag(&answer);
                let mut value = done(checked, "the new name");
                value["album"] = show_album(&album);
                Ok((value, Settled::Album(album)))
            }
        }
    }
}

/// The tool's entry in `tools/list`.
pub(crate) fn tool() -> Value {
    let id = json!({"description":"Photo id from photos_list, photos_search or photos_get.","type":"string","minLength":1,"maxLength":crate::photos::MAX_ID_BYTES});
    json!({
        "name": TOOL,
        "description": "Change the user's Apple iCloud Photos (personal library): upload (local, a photo or video file in the exchange folder the user set up in their terminal), set_favorite (id, favorite), set_title_and_caption (id, title and/or caption; empty text clears), set_rating (id, rating 0 to 5), set_keywords (id, keywords: the whole new list; the words added or taken away are merged into Apple's list as it is at that moment, so a word another device adds at the same time stays), set_date (id, taken_at such as 2024-05-01T14:30:00+02:00), create_album (name), rename_album (album_id, name) and trash (id; moves the photo to Recently Deleted). Photos and albums are found in the local cache, so run photos_refresh first. A photo or album changed at Apple since the cache read it is refused, never written over; run photos_refresh and ask again. trash always needs the user's approval, in the approval form or with skykeep-mcp approve in their terminal; every change follows the approval mode. Nothing is deleted for good, no picture is edited, and shared albums are not touched. The reply says done with what Apple's answer showed, or an error; an unknown outcome means run photos_refresh and look before trying again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "id": id,
                "local": {"description": "upload only: a file inside the exchange folder, as a relative path such as IMG_0001.HEIC or in/clip.mov.", "type": "string", "minLength": 1, "maxLength": exchange::MAX_LOCAL_BYTES},
                "favorite": {"description": "set_favorite only: true marks the photo as a favorite, false takes the mark away.", "type": "boolean"},
                "title": {"description": "set_title_and_caption only: the new title; empty clears it.", "type": "string", "maxLength": MAX_TITLE_CHARS * 4},
                "caption": {"description": "set_title_and_caption only: the new caption; empty clears it.", "type": "string", "maxLength": MAX_CAPTION_CHARS * 4},
                "rating": {"description": "set_rating only: stars, 0 (none) to 5.", "type": "integer", "minimum": 0, "maximum": 5},
                "keywords": {"description": "set_keywords only: the whole new list of keywords; an empty list removes them all.", "type": "array", "maxItems": MAX_KEYWORDS, "items": {"type": "string", "minLength": 1, "maxLength": MAX_KEYWORD_CHARS * 4}},
                "taken_at": {"description": "set_date only: when the photo was taken, YYYY-MM-DDTHH:MM:SS with Z or an offset such as +02:00.", "type": "string", "minLength": 20, "maxLength": 25},
                "name": {"description": "create_album and rename_album: the album's name.", "type": "string", "minLength": 1, "maxLength": MAX_ALBUM_NAME_CHARS * 4},
                "album_id": {"description": "rename_album only: album id from photos_albums.", "type": "string", "minLength": 1, "maxLength": crate::photos::MAX_ID_BYTES},
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
    fn only_trash_is_a_removal_and_it_is_recoverable() {
        for action in ACTIONS {
            assert_eq!(is_destructive(action), action == "trash", "{action}");
        }
        assert_eq!(
            crate::permissions::removal_of(TOOL, "trash"),
            crate::permissions::Removal::Recoverable
        );
    }

    #[test]
    fn moments_parse_with_zone_or_offset_and_refuse_anything_else() {
        assert_eq!(parse_moment("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_moment("1970-01-01T02:00:00+02:00"), Ok(0));
        assert_eq!(parse_moment("1969-12-31T19:30:00-04:30"), Ok(0));
        assert_eq!(parse_moment("2024-02-29T12:00:00Z"), Ok(1_709_208_000));
        for bad in [
            "2023-02-29T12:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-01-01 00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:00",
            "2024-01-01T00:00:00+2:00",
            "1800-01-01T00:00:00Z",
            "２０２４-01-01T00:00:00Z",
            "2024-01-01T00:00:00+19:00",
        ] {
            assert_eq!(parse_moment(bad), Err("invalid_date"), "{bad}");
        }
    }

    #[test]
    fn the_types_named_are_the_ones_the_engine_takes() {
        for ext in [
            "jpg", "jpeg", "heic", "heif", "png", "gif", "tif", "tiff", "webp", "dng", "mov",
            "mp4", "m4v", "avi",
        ] {
            assert_eq!(upload::refusal(&format!("a.{ext}"), 1000), None, "{ext}");
            assert!(
                UPLOAD_TYPES.to_lowercase().contains(ext),
                "{ext} in {UPLOAD_TYPES}"
            );
        }
    }

    #[test]
    fn base64_matches_what_photos_writes() {
        assert_eq!(base64_text(""), "");
        assert_eq!(base64_text("f"), "Zg==");
        assert_eq!(base64_text("fo"), "Zm8=");
        assert_eq!(base64_text("foo"), "Zm9v");
        assert_eq!(base64_text("Strand é"), "U3RyYW5kIMOp");
    }

    #[test]
    fn arguments_are_bounded_and_checked_before_anything_else() {
        let ok = |args: Value| validate(&args).map(|_| ());
        assert_eq!(
            ok(json!({"action":"set_rating","id":"A","rating":5})),
            Ok(())
        );
        assert_eq!(
            ok(json!({"action":"set_rating","id":"A","rating":6})),
            Err("invalid_rating")
        );
        assert_eq!(
            ok(json!({"action":"set_favorite","id":"A","favorite":true,"extra":1})),
            Err("invalid_arguments")
        );
        assert_eq!(
            ok(json!({"action":"delete_forever","id":"A"})),
            Err("invalid_arguments")
        );
        assert_eq!(
            ok(json!({"action":"set_title_and_caption","id":"A"})),
            Err("title_or_caption_required")
        );
        assert_eq!(
            ok(json!({"action":"set_title_and_caption","id":"A","title":"x".repeat(256)})),
            Err("invalid_title")
        );
        assert_eq!(
            ok(json!({"action":"set_title_and_caption","id":"A","title":"a\u{202E}b"})),
            Err("invalid_title")
        );
        assert_eq!(
            ok(json!({"action":"set_title_and_caption","id":"A","caption":"line\nline"})),
            Ok(())
        );
        assert_eq!(
            ok(json!({"action":"set_keywords","id":"A","keywords":vec!["k"; 51]})),
            Err("too_many_keywords")
        );
        assert_eq!(
            ok(json!({"action":"set_keywords","id":"A","keywords":["Beach","beach"]})),
            Err("duplicate_keyword")
        );
        assert_eq!(
            ok(json!({"action":"set_keywords","id":"A","keywords":[" pad"]})),
            Err("invalid_keyword")
        );
        assert_eq!(
            ok(json!({"action":"set_date","id":"A","taken_at":"yesterday"})),
            Err("invalid_date")
        );
        assert_eq!(
            ok(json!({"action":"create_album","name":" "})),
            Err("invalid_album_name")
        );
        assert_eq!(
            ok(json!({"action":"trash","id":"x".repeat(513)})),
            Err("invalid_arguments")
        );
        assert_eq!(
            ok(json!({"action":"upload","local":"/etc/passwd.jpg"})),
            Err("absolute_local_path_refused")
        );
        assert_eq!(
            ok(json!({"action":"upload","local":"../a.jpg"})),
            Err("local_path_traversal_refused")
        );
        assert_eq!(
            ok(json!({"action":"upload","local":"notes.txt"})),
            Err("photos_file_type_not_supported")
        );
    }
}
