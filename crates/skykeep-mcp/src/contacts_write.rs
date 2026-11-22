//! `contacts_write`: changes to Apple iCloud Contacts, one action per call: `create`, `update`
//! and `delete` of one card.
//!
//! Contacts changes are not queued here: an action runs as soon as it is allowed, and the
//! reply says what happened. Done means Apple took the write, and `remote_verified` says
//! whether the book read back from Apple afterwards shows it. A connection that ended before
//! Apple's answer leaves the outcome unknown. Nothing is retried on its own.
//!
//! An update or delete names the card's version, the `etag` `contacts_get` shows. It must be
//! the cached one (so the approval shows the card that will change) and still Apple's: the
//! card is read from Apple first, and a card changed since is refused as a conflict, never
//! written over. The write itself carries that version, so Apple refuses it too if the card
//! moves on in between. An update writes only the fields it names, laid over Apple's current
//! card; everything else on the card (addresses, photo, fields the cache does not keep) stays
//! as Apple has it. Contact lists, photos and merging cards are not part of this tool.
//!
//! Who approves. A delete is permanent ([`is_destructive`], from the permissions rules), so
//! it always needs the person: on the card in the MCP client under `reads_and_writes` and
//! `writes_only`, or with `skykeep-mcp approve` in their terminal under `yolo` or with no
//! approval mode. No rule lets it run unasked, and its card offers no "Always allow".
use crate::account::{binding, Binding};
use crate::contacts::{self, RefreshLimits, Stop};
use crate::drive_write::{shown, MakeClient};
use crate::exchange::hides_text;
use quince_auth::{Client, Endpoints};
use quince_core::contacts::{Card, ContactMethod};
use quince_core::Library;
use quince_icloud::contacts as apple;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub const TOOL: &str = "contacts_write";
/// The actions, as the tool's schema names them.
pub const ACTIONS: [&str; 3] = ["create", "update", "delete"];

/// Characters in a name, company, birthday or name and work detail.
pub const MAX_TEXT_CHARS: usize = 256;
/// Characters in the notes.
pub const MAX_NOTES_CHARS: usize = 4096;
/// Entries in each of emails, phones and URLs.
pub const MAX_ENTRIES: usize = 20;
/// Characters in one entry's value, and in its label.
pub const MAX_VALUE_CHARS: usize = 512;
pub const MAX_LABEL_CHARS: usize = 64;
const MAX_ETAG_BYTES: usize = 256;

/// The card's version is not the one given: changed at Apple, or since it was read.
pub const CHANGED: &str = "contact_changed_conflict";
pub const UNKNOWN: &str = "contacts_write_outcome_unknown_run_contacts_refresh";
const NOT_CACHED: &str = "contact_not_in_cache_run_contacts_refresh";
const GONE: &str = "contact_not_found_at_apple_run_contacts_refresh";
const PAUSED: &str = "contacts_paused_approve_on_apple_device";
const BLANK: &str = "contact_needs_a_name_phone_or_email";

/// The Contacts actions that remove something, as the permissions rules decide it for every
/// tool (`permissions::removal_of`): `delete`, which is permanent.
pub fn is_destructive(action: &str) -> bool {
    crate::permissions::is_deletion(TOOL, action)
}

/// Accepts a present value, `null` included, as `Some`: absent stays `None` by default, so
/// `Some(None)` is a field to clear.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    #[serde(default)]
    label: Option<String>,
    value: String,
}

/// The fields a create sets or an update changes. A field left out is left as it is; `null`
/// or an empty string clears it. A list given replaces that kind of entry whole.
#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fields {
    #[serde(default, deserialize_with = "present")]
    first_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    middle_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    last_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    company: Option<Option<String>>,
    #[serde(default)]
    is_company: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    birthday: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    notes: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    prefix: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    suffix: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    nickname: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    phonetic_first_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    phonetic_last_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    phonetic_company: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    job_title: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    department: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    emails: Option<Option<Vec<Entry>>>,
    #[serde(default, deserialize_with = "present")]
    phones: Option<Option<Vec<Entry>>>,
    #[serde(default, deserialize_with = "present")]
    urls: Option<Option<Vec<Entry>>>,
}

/// The name and work details by the tool's names, with the key Apple's card uses.
const DETAILS: [(&str, &str); 8] = [
    ("prefix", "prefix"),
    ("suffix", "suffix"),
    ("nickname", "nickName"),
    ("phonetic_first_name", "phoneticFirstName"),
    ("phonetic_last_name", "phoneticLastName"),
    ("phonetic_company", "phoneticCompanyName"),
    ("job_title", "jobTitle"),
    ("department", "department"),
];

impl Fields {
    /// Every text field given, by the tool's name, with its limit.
    fn texts(&self) -> Vec<(&'static str, &Option<String>, usize)> {
        let mut out = Vec::new();
        for (name, value, cap) in [
            ("first_name", &self.first_name, MAX_TEXT_CHARS),
            ("middle_name", &self.middle_name, MAX_TEXT_CHARS),
            ("last_name", &self.last_name, MAX_TEXT_CHARS),
            ("company", &self.company, MAX_TEXT_CHARS),
            ("birthday", &self.birthday, MAX_TEXT_CHARS),
            ("notes", &self.notes, MAX_NOTES_CHARS),
        ] {
            if let Some(value) = value {
                out.push((name, value, cap));
            }
        }
        for (name, _) in DETAILS {
            if let Some(value) = self.detail(name) {
                out.push((name, value, MAX_TEXT_CHARS));
            }
        }
        out
    }

    fn detail(&self, name: &str) -> Option<&Option<String>> {
        match name {
            "prefix" => self.prefix.as_ref(),
            "suffix" => self.suffix.as_ref(),
            "nickname" => self.nickname.as_ref(),
            "phonetic_first_name" => self.phonetic_first_name.as_ref(),
            "phonetic_last_name" => self.phonetic_last_name.as_ref(),
            "phonetic_company" => self.phonetic_company.as_ref(),
            "job_title" => self.job_title.as_ref(),
            "department" => self.department.as_ref(),
            _ => None,
        }
    }

    /// Each list given, with the method kind the cache names it by.
    fn lists(&self) -> Vec<(&'static str, &'static str, Vec<Entry>)> {
        [
            ("emails", "email", &self.emails),
            ("phones", "phone", &self.phones),
            ("urls", "url", &self.urls),
        ]
        .into_iter()
        .filter_map(|(name, kind, list)| {
            list.as_ref()
                .map(|l| (name, kind, l.clone().unwrap_or_default()))
        })
        .collect()
    }

    fn is_empty(&self) -> bool {
        self.texts().is_empty() && self.lists().is_empty() && self.is_company.is_none()
    }

    /// The names of the fields given, for the preview.
    fn named(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (name, value, _) in self.texts() {
            out.insert(name.into(), json!(value.as_deref().map(|v| shown(v, 256))));
        }
        if let Some(is_company) = self.is_company {
            out.insert("is_company".into(), json!(is_company));
        }
        for (name, _, list) in self.lists() {
            let shown_list: Vec<Value> = list
                .iter()
                .map(|e| json!({"label": e.label.as_deref().map(|l| shown(l, MAX_LABEL_CHARS)), "value": shown(&e.value, MAX_VALUE_CHARS)}))
                .collect();
            out.insert(name.into(), json!(shown_list));
        }
        Value::Object(out)
    }

    /// `card` with these fields laid over it.
    fn apply(&self, card: &mut Card) {
        let set = |slot: &mut Option<String>, value: &Option<Option<String>>| {
            if let Some(value) = value {
                *slot = clean(value.as_deref());
            }
        };
        let c = &mut card.contact;
        set(&mut c.first_name, &self.first_name);
        set(&mut c.middle_name, &self.middle_name);
        set(&mut c.last_name, &self.last_name);
        set(&mut c.company, &self.company);
        set(&mut c.birthday, &self.birthday);
        set(&mut c.notes, &self.notes);
        if let Some(is_company) = self.is_company {
            c.is_company = is_company;
        }
        for (name, key) in DETAILS {
            if let Some(value) = self.detail(name) {
                card.details.set(key, value.as_deref());
            }
        }
        for (_, kind, list) in self.lists() {
            let c = &mut card.contact;
            c.methods.retain(|m| m.kind != kind);
            c.methods.extend(list.into_iter().map(|e| ContactMethod {
                kind: kind.to_string(),
                label: clean(e.label.as_deref()),
                value: e.value.trim().to_string(),
            }));
        }
    }

    /// The fields given that `card` does not hold as asked.
    fn differences(&self, card: &Card) -> Vec<&'static str> {
        let mut wanted = card.clone();
        self.apply(&mut wanted);
        let mut out = Vec::new();
        let c = (&card.contact, &wanted.contact);
        for (name, have, want) in [
            ("first_name", &c.0.first_name, &c.1.first_name),
            ("middle_name", &c.0.middle_name, &c.1.middle_name),
            ("last_name", &c.0.last_name, &c.1.last_name),
            ("company", &c.0.company, &c.1.company),
            ("birthday", &c.0.birthday, &c.1.birthday),
            ("notes", &c.0.notes, &c.1.notes),
        ] {
            if have != want {
                out.push(name);
            }
        }
        if c.0.is_company != c.1.is_company {
            out.push("is_company");
        }
        for (name, key) in DETAILS {
            if card.details.get(key) != wanted.details.get(key) {
                out.push(name);
            }
        }
        for (name, kind, _) in self.lists() {
            let of = |card: &Card| -> Vec<(String, Option<String>)> {
                card.contact
                    .methods
                    .iter()
                    .filter(|m| m.kind == kind)
                    .map(|m| (m.value.clone(), m.label.clone()))
                    .collect()
            };
            if of(card) != of(&wanted) {
                out.push(name);
            }
        }
        out
    }
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ContactsWrite {
    Create {
        fields: Fields,
    },
    Update {
        guid: String,
        etag: String,
        fields: Fields,
    },
    Delete {
        guid: String,
        etag: String,
    },
}

impl ContactsWrite {
    pub(crate) fn action(&self) -> &'static str {
        match self {
            ContactsWrite::Create { .. } => "create",
            ContactsWrite::Update { .. } => "update",
            ContactsWrite::Delete { .. } => "delete",
        }
    }

    fn target(&self) -> Option<(&str, &str)> {
        match self {
            ContactsWrite::Create { .. } => None,
            ContactsWrite::Update { guid, etag, .. } | ContactsWrite::Delete { guid, etag } => {
                Some((guid, etag))
            }
        }
    }
}

/// Text a person can read as given: no control characters (a line break or tab only in the
/// notes) and none that hide or reorder text.
fn readable(text: &str, multiline: bool) -> bool {
    !text
        .chars()
        .any(|c| hides_text(c) || (c.is_control() && !(multiline && matches!(c, '\n' | '\t'))))
}

/// A birthday as Apple's card holds it: `YYYY-MM-DD`.
fn birthday(text: &str) -> bool {
    let b = text.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || !digits(0..4) {
        return false;
    }
    if !digits(5..7) || !digits(8..10) {
        return false;
    }
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn check_fields(fields: &Fields) -> Result<(), &'static str> {
    for (name, value, cap) in fields.texts() {
        let Some(value) = value else { continue };
        if value.chars().count() > cap {
            return Err("field_too_long");
        }
        if !readable(value, name == "notes") {
            return Err("invalid_text");
        }
        if name == "birthday" && !value.trim().is_empty() && !birthday(value.trim()) {
            return Err("invalid_birthday");
        }
    }
    for (_, _, list) in fields.lists() {
        if list.len() > MAX_ENTRIES {
            return Err("too_many_entries");
        }
        for entry in &list {
            if entry.value.chars().count() > MAX_VALUE_CHARS
                || entry
                    .label
                    .as_ref()
                    .is_some_and(|l| l.chars().count() > MAX_LABEL_CHARS)
            {
                return Err("field_too_long");
            }
            if !readable(&entry.value, false)
                || entry.label.as_ref().is_some_and(|l| !readable(l, false))
            {
                return Err("invalid_text");
            }
            if entry.value.trim().is_empty() {
                return Err("empty_entry_value");
            }
        }
    }
    Ok(())
}

/// Checks the arguments before any approval prompt, cache or network use.
pub(crate) fn validate(args: &Value) -> Result<ContactsWrite, &'static str> {
    let op: ContactsWrite =
        serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
    if let Some((guid, etag)) = op.target() {
        if guid.is_empty() || guid.len() > contacts::MAX_GUID_BYTES {
            return Err("invalid_arguments");
        }
        if etag.is_empty() || etag.len() > MAX_ETAG_BYTES || !readable(etag, false) {
            return Err("invalid_arguments");
        }
    }
    match &op {
        ContactsWrite::Create { fields } => {
            check_fields(fields)?;
            let mut card = blank_card();
            fields.apply(&mut card);
            if apple::blank_card(&card.contact) {
                return Err(BLANK);
            }
        }
        ContactsWrite::Update { fields, .. } => {
            check_fields(fields)?;
            if fields.is_empty() {
                return Err("nothing_to_change");
            }
        }
        ContactsWrite::Delete { .. } => {}
    }
    Ok(op)
}

/// Whether a call needs the person's approval whatever the rules and the approval mode say:
/// a delete. Arguments that don't validate are left to the tool, which refuses them.
pub(crate) fn needs_person(name: &str, args: &Value) -> bool {
    name == TOOL && validate(args).is_ok_and(|op| is_destructive(op.action()))
}

fn blank_card() -> Card {
    Card {
        contact: quince_core::contacts::Contact {
            guid: String::new(),
            first_name: None,
            middle_name: None,
            photo_url: None,
            photo_signature: None,
            photo_crop: None,
            last_name: None,
            company: None,
            is_company: false,
            birthday: None,
            notes: None,
            etag: None,
            methods: Vec::new(),
        },
        details: Default::default(),
    }
}

// --- The cache.

/// The cached card an update or delete names, at the version given.
fn cached(library: &Library, guid: &str, etag: &str) -> Result<Card, &'static str> {
    let card = library
        .cards_by_guid(&[guid.to_string()])
        .map_err(|_| "library_unavailable")?
        .into_iter()
        .find(|c| c.contact.guid == guid)
        .ok_or(NOT_CACHED)?;
    if card.contact.etag.as_deref() != Some(etag) {
        return Err(CHANGED);
    }
    Ok(card)
}

fn show_card(card: &Card) -> Value {
    let c = &card.contact;
    let count = |kind: &str| c.methods.iter().filter(|m| m.kind == kind).count();
    json!({
        "guid": c.guid,
        "display_name": shown(&c.display_name(), 256),
        "first_name": c.first_name.as_deref().map(|v| shown(v, 256)),
        "last_name": c.last_name.as_deref().map(|v| shown(v, 256)),
        "company": c.company.as_deref().map(|v| shown(v, 256)),
        "etag": c.etag,
        "emails": count("email"),
        "phones": count("phone"),
    })
}

/// What the approval shows and is bound to: the action, the cached card it reaches, and the
/// fields it sets. The same arguments over the same cache give the same preview.
pub(crate) fn preview(library: &Library, args: &Value) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
    });
    if let Some((guid, etag)) = op.target() {
        value["contact"] = show_card(&cached(library, guid, etag)?);
    }
    match &op {
        ContactsWrite::Create { fields } => {
            value["fields"] = fields.named();
            value["effect"] = json!("adds one new card to Apple iCloud Contacts");
        }
        ContactsWrite::Update { fields, .. } => {
            value["fields"] = fields.named();
            value["effect"] = json!("changes only these fields on the card at Apple, if it is still at this version; the rest of the card stays as it is");
        }
        ContactsWrite::Delete { .. } => {
            value["effect"] = json!("deletes the card from Apple iCloud Contacts on every device; it cannot be undone here");
        }
    }
    Ok(value)
}

/// A preview in plain lines, for the terminal approval.
pub(crate) fn plain_preview(preview: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or("?").to_string();
    let mut lines = vec![format!(
        "Contacts change: {}{}",
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
    let contact = &preview["contact"];
    if contact.is_object() {
        lines.push(format!(
            "Contact: {} (id {}, version {})",
            text(&contact["display_name"]),
            text(&contact["guid"]),
            text(&contact["etag"])
        ));
    }
    if let Some(fields) = preview["fields"].as_object() {
        for (name, value) in fields {
            lines.push(format!("Set {name}: {value}"));
        }
    }
    lines.join("\n")
}

/// The terminal approval's text for stored arguments, as the cache shows them now.
pub(crate) fn approval_text(library: &Library, args: &Value) -> String {
    match preview(library, args) {
        Ok(preview) => plain_preview(&preview),
        Err(error) => format!("Contacts change: {args}\nThe cache can't show it now: {error}"),
    }
}

/// What the person can do after `error`, when there is something.
pub(crate) fn next_step(error: &str) -> Option<String> {
    Some(match error {
        CHANGED => "The card is not at the version given: it changed on another device or since it was read. Nothing was written. Run contacts_refresh, read it again with contacts_get, and ask again with its new etag.",
        UNKNOWN => "The connection ended before Apple answered, so the change may or may not have been made. Run contacts_refresh and look at the card before trying again.",
        NOT_CACHED => "The card is not in the local cache. Run contacts_refresh, then find it with contacts_search or contacts_get.",
        GONE => "Apple no longer has this card; it may have been deleted on another device. Run contacts_refresh.",
        BLANK => "A new card needs a name, a company, a phone number or an email address.",
        _ => return None,
    }
    .to_string())
}

// --- Running an action.

/// Test seam: synthetic endpoints and other limits. Production uses neither.
#[doc(hidden)]
#[derive(Clone)]
pub struct Seam {
    pub client: Option<MakeClient>,
    /// Each read of the book, before and after the change.
    pub read: RefreshLimits,
    /// The whole action.
    pub timeout: Duration,
}

impl Default for Seam {
    fn default() -> Self {
        Self {
            client: None,
            read: RefreshLimits::default(),
            timeout: Duration::from_secs(120),
        }
    }
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

fn apple_error(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::ReauthRequired => "login_required",
        E::NeedsDeviceApproval => PAUSED,
        E::Conflict => CHANGED,
        E::Refused(_) => "contacts_write_refused_by_apple",
        E::Protocol(_) => "contacts_write_refused_by_apple",
        _ => UNKNOWN,
    }
}

fn read_stop(stop: Stop) -> &'static str {
    match stop {
        Stop::Reauth => "login_required",
        Stop::TooLarge | Stop::BookTooLarge | Stop::CardTooLarge => {
            "contacts_book_too_large_nothing_sent"
        }
        Stop::Busy | Stop::Failed => "contacts_read_failed_nothing_sent",
    }
}

/// Apple's book now: its cards as sent, and the tokens a write carries.
async fn book(
    client: &Client,
    root: &str,
    limits: &RefreshLimits,
) -> Result<(Vec<Value>, apple::Tokens), Stop> {
    let mut budget = limits.max_total_bytes;
    let (mut body, tokens) = contacts::raw_book(client, root, limits, &mut budget).await?;
    let cards = match body.get_mut("contacts").map(Value::take) {
        Some(Value::Array(cards)) => cards,
        _ => Vec::new(),
    };
    Ok((cards, tokens))
}

fn find(cards: &[Value], guid: &str) -> Option<Value> {
    cards
        .iter()
        .find(|c| c.get("contactId").and_then(Value::as_str) == Some(guid))
        .cloned()
}

/// One of Apple's cards as the cache reads it, with its addresses part by part.
fn parsed(card: &Value) -> Option<(Card, Vec<quince_core::contacts::PostalAddress>)> {
    let body = json!({ "contacts": [card] });
    let contact = apple::parse(&body).into_iter().next()?;
    let details = apple::parse_details(&body)
        .into_iter()
        .next()
        .map(|(_, d)| d)
        .unwrap_or_default();
    let addresses = apple::parse_addresses(&body)
        .into_iter()
        .next()
        .map(|(_, a)| a)
        .unwrap_or_default();
    Some((Card { contact, details }, addresses))
}

/// What the cache does after the change, only while the directory still belongs to the
/// account the change ran as.
enum Settle {
    Nothing,
    Store(Box<Card>, Vec<quince_core::contacts::PostalAddress>),
    Forget(String),
}

fn settle(dir: &Path, dsid: &str, what: Settle) -> bool {
    let Ok(mut library) = Library::open(dir) else {
        return false;
    };
    if binding(&library, dsid) != Ok(Binding::Same) {
        return false;
    }
    match what {
        Settle::Nothing => false,
        Settle::Forget(guid) => library.forget_contact(&guid).is_ok(),
        Settle::Store(card, addresses) => {
            library.put_card(&card).is_ok()
                && library
                    .set_contact_addresses(&card.contact.guid, &addresses)
                    .is_ok()
        }
    }
}

/// Runs one action that is allowed to run: the caller has checked the approval. The caller
/// holds the private directory's lock.
pub(crate) async fn run(dir: &Path, seam: &Seam, op: ContactsWrite) -> Result<Value, &'static str> {
    {
        let library = Library::open(dir).map_err(|_| "library_unavailable")?;
        if let Some((guid, etag)) = op.target() {
            cached(&library, guid, etag)?;
        }
    }
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
    {
        let library = Library::open(dir).map_err(|_| "library_unavailable")?;
        if binding(&library, &dsid)? != Binding::Same {
            return Err("account_mismatch_use_new_state_directory");
        }
    }
    let root = client
        .session()
        .service_url("contacts")
        .ok_or("contacts_service_unavailable")?;
    let mut after = Settle::Nothing;
    let result = match tokio::time::timeout(
        seam.timeout,
        act(&client, &root, &op, &seam.read, &mut after),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(UNKNOWN),
    };
    let cached = settle(dir, &dsid, after);
    result.map(|mut value| {
        value["action"] = json!(op.action());
        value["cache_updated"] = json!(cached);
        if !cached && value["outcome"] == "done" {
            value["cache"] = json!(
                "the local cache was not updated; run contacts_refresh before reading this card"
            );
        }
        value
    })
}

async fn act(
    client: &Client,
    root: &str,
    op: &ContactsWrite,
    limits: &RefreshLimits,
    after: &mut Settle,
) -> Result<Value, &'static str> {
    let http = client.http();
    let params = || client.service_params();
    let (guid, fields) = match op {
        ContactsWrite::Create { fields } => {
            let guid = apple::new_contact_id();
            let mut card = blank_card();
            fields.apply(&mut card);
            card.contact.guid = guid.clone();
            let ledger = apple::Ledger::new();
            apple::create_card_named(http, root, params(), &ledger, &guid, &card)
                .await
                .map_err(|e| apple_error(&e))?;
            (guid, Some(fields))
        }
        ContactsWrite::Update { guid, etag, fields } => {
            let (cards, tokens) = book(client, root, limits).await.map_err(read_stop)?;
            let existing = find(&cards, guid).ok_or(GONE)?;
            drop(cards);
            if existing.get("etag").and_then(Value::as_str) != Some(etag.as_str()) {
                return Err(CHANGED);
            }
            let (base, _) = parsed(&existing).ok_or("contacts_card_unreadable_nothing_sent")?;
            let mut card = base.clone();
            fields.apply(&mut card);
            if fields.differences(&base).is_empty() {
                return Ok(json!({
                    "outcome": "unchanged",
                    "contact": {"guid": guid, "etag": etag},
                    "checked": "Apple's card, read just now, already holds these values; nothing was sent",
                    "remote_verified": true,
                }));
            }
            let ledger = apple::Ledger::holding(tokens);
            apple::update_card(
                http,
                root,
                params(),
                &ledger,
                &card,
                Some(&existing),
                Some(&base),
            )
            .await
            .map_err(|e| apple_error(&e))?;
            (guid.clone(), Some(fields))
        }
        ContactsWrite::Delete { guid, etag } => {
            let (cards, tokens) = book(client, root, limits).await.map_err(read_stop)?;
            let existing = find(&cards, guid).ok_or(GONE)?;
            drop(cards);
            if existing.get("etag").and_then(Value::as_str) != Some(etag.as_str()) {
                return Err(CHANGED);
            }
            // Sent against the version just checked, so Apple refuses it if the card moves
            // on in between. The engine's `delete` would send Apple's newest version instead.
            let ledger = apple::Ledger::holding(tokens);
            apple::merge::delete_card(http, root, params(), &ledger, &existing)
                .await
                .map_err(|e| apple_error(&e))?;
            (guid.clone(), None)
        }
    };
    // Apple took the write. The book is read back to see it there.
    let unverified = |why: &str| {
        json!({
            "outcome": "done",
            "contact": {"guid": guid},
            "checked": format!("Apple accepted the write, but {why}"),
            "remote_verified": false,
        })
    };
    let cards = match book(client, root, limits).await {
        Ok((cards, _)) => cards,
        Err(_) => {
            return Ok(unverified(
                "the book could not be read back to check it; run contacts_refresh",
            ))
        }
    };
    let found = find(&cards, &guid);
    drop(cards);
    let Some(fields) = fields else {
        if found.is_some() {
            return Ok(unverified("the book read back still holds the card"));
        }
        *after = Settle::Forget(guid.clone());
        return Ok(json!({
            "outcome": "done",
            "contact": {"guid": guid},
            "checked": "the book was read back from Apple and no longer holds the card",
            "remote_verified": true,
        }));
    };
    let Some((card, addresses)) = found.as_ref().and_then(parsed) else {
        return Ok(unverified("the book read back does not hold the card"));
    };
    let differ = fields.differences(&card);
    let reply = json!({
        "outcome": "done",
        "contact": {"guid": guid, "etag": card.contact.etag, "display_name": shown(&card.contact.display_name(), 256)},
        "checked": if differ.is_empty() {
            "the book was read back from Apple and the card holds every field as asked".to_string()
        } else {
            format!("the book was read back from Apple, and these fields read differently than asked (Apple may have reformatted them): {}", differ.join(", "))
        },
        "differences": differ,
        "remote_verified": differ.is_empty(),
    });
    *after = Settle::Store(Box::new(card), addresses);
    Ok(reply)
}

/// The tool's entry in `tools/list`.
pub(crate) fn tool() -> Value {
    let text = |what: &str| json!({"description": what, "type": ["string", "null"], "maxLength": MAX_TEXT_CHARS});
    let entries = |what: &str| {
        json!({
            "description": what,
            "type": ["array", "null"],
            "maxItems": MAX_ENTRIES,
            "items": {
                "type": "object",
                "properties": {
                    "label": {"description": "Apple's label, such as HOME, WORK, MOBILE or OTHER, or your own words.", "type": ["string", "null"], "maxLength": MAX_LABEL_CHARS},
                    "value": {"type": "string", "minLength": 1, "maxLength": MAX_VALUE_CHARS},
                },
                "required": ["value"],
                "additionalProperties": false,
            },
        })
    };
    let mut fields = serde_json::Map::new();
    for (name, what) in [
        ("first_name", "First name."),
        ("middle_name", "Middle name."),
        ("last_name", "Last name."),
        ("company", "Company."),
        ("birthday", "Birthday as YYYY-MM-DD."),
        ("prefix", "Name prefix, such as Dr."),
        ("suffix", "Name suffix, such as Jr."),
        ("nickname", "Nickname."),
        ("phonetic_first_name", "Phonetic first name."),
        ("phonetic_last_name", "Phonetic last name."),
        ("phonetic_company", "Phonetic company name."),
        ("job_title", "Job title."),
        ("department", "Department."),
    ] {
        fields.insert(name.into(), text(what));
    }
    fields.insert(
        "notes".into(),
        json!({"description": "Notes; line breaks allowed.", "type": ["string", "null"], "maxLength": MAX_NOTES_CHARS}),
    );
    fields.insert(
        "is_company".into(),
        json!({"description": "Whether the card is a company's.", "type": "boolean"}),
    );
    fields.insert(
        "emails".into(),
        entries("Every email address of the card; replaces the ones it has."),
    );
    fields.insert(
        "phones".into(),
        entries("Every phone number of the card; replaces the ones it has."),
    );
    fields.insert(
        "urls".into(),
        entries("Every web address of the card; replaces the ones it has."),
    );
    json!({
        "name": TOOL,
        "description": "Change the user's Apple iCloud Contacts, one card per call: create (fields), update (guid, etag, fields) or delete (guid, etag). Take guid and etag from contacts_get; the etag is the card's version. An update or delete of a card that changed since, on another device or in the cache, is refused with contact_changed_conflict and nothing is written: run contacts_refresh, read the card again and ask with its new etag. An update changes only the fields it names; null or an empty string clears a field, and a list of emails, phones or URLs replaces that list. Postal addresses, photos, contact lists and merging cards are not changed by this tool. Deletes are permanent and always need the user's approval, in the approval form or with skykeep-mcp approve in their terminal; every change follows the approval mode and the rules the user set. The reply says done with what was checked, unchanged when Apple already holds the values, or an error; an unknown outcome means run contacts_refresh and look before trying again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "guid": {"description": "Contact GUID from contacts_list, contacts_search or contacts_get. update and delete only.", "type": "string", "minLength": 1, "maxLength": contacts::MAX_GUID_BYTES},
                "etag": {"description": "The card's version, the etag contacts_get shows. update and delete only.", "type": "string", "minLength": 1, "maxLength": MAX_ETAG_BYTES},
                "fields": {
                    "description": "create and update only. A field left out stays as it is.",
                    "type": "object",
                    "properties": fields,
                    "additionalProperties": false,
                },
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
    fn only_delete_is_destructive_and_arguments_are_strict() {
        for action in ACTIONS {
            assert_eq!(is_destructive(action), action == "delete");
        }
        assert!(validate(&json!({"action":"delete","guid":"g","etag":"e"})).is_ok());
        assert_eq!(
            validate(&json!({"action":"delete","guid":"g","etag":"e","fields":{}})),
            Err("invalid_arguments")
        );
        assert_eq!(
            validate(&json!({"action":"create","fields":{"photo":"x"}})),
            Err("invalid_arguments")
        );
    }

    #[test]
    fn null_clears_and_absent_keeps() {
        let op = validate(&json!({"action":"update","guid":"g","etag":"e","fields":{"company":null,"first_name":"  Ada "}})).unwrap();
        let ContactsWrite::Update { fields, .. } = op else {
            panic!()
        };
        let mut card = blank_card();
        card.contact.company = Some("Old".into());
        card.contact.last_name = Some("Kept".into());
        fields.apply(&mut card);
        assert_eq!(card.contact.company, None);
        assert_eq!(card.contact.first_name.as_deref(), Some("Ada"));
        assert_eq!(card.contact.last_name.as_deref(), Some("Kept"));
        assert!(fields.differences(&card).is_empty());
    }

    #[test]
    fn birthdays_are_dates() {
        for good in ["1990-03-14", "2000-12-31"] {
            assert!(birthday(good), "{good}");
        }
        for bad in [
            "1990-13-01",
            "1990-3-14",
            "14/03/1990",
            "--03-14",
            "1990-00-10",
        ] {
            assert!(!birthday(bad), "{bad}");
        }
    }
}
