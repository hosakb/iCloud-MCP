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
//! it always needs the person, on the card in the MCP client in every approval mode (or with
//! `skykeep-mcp approve` where an `ask: terminal` rule asks for that). No rule lets it run
//! unasked, and its card offers no "Always allow".
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
const PAUSED: &str = "contacts_write_paused_approve_on_apple_device";
const REFUSED: &str = "contacts_write_refused_by_apple";
const TOO_LARGE: &str = "contacts_write_book_too_large_nothing_sent";
const UNREADABLE: &str = "contacts_write_card_unreadable_nothing_sent";
const CREATE_REFUSED: &str = "contact_create_refused_try_again";
/// A stored terminal request no longer matches the cache.
pub const OUT_OF_DATE: &str = "contacts_write_request_out_of_date";
const BLANK: &str = "contact_needs_a_name_phone_or_email";
const CUT_SHORT: &str = "contact_shown_cut_short_edit_on_device";
const NOTHING_SENT: &str = "contacts_write_read_failed_nothing_sent";

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

    /// The fields given that `card` does not hold as asked. A list is compared without regard
    /// to order, as Apple keeps it: entries reordered, or the same entry given twice where the
    /// card has it twice, are no difference.
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
                let mut entries: Vec<_> = card
                    .contact
                    .methods
                    .iter()
                    .filter(|m| m.kind == kind)
                    .map(|m| (m.value.clone(), m.label.clone()))
                    .collect();
                entries.sort();
                entries
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
/// notes), none that hide or reorder text, and none that can't be seen.
fn readable(text: &str, multiline: bool) -> bool {
    !text.chars().any(|c| {
        hides_text(c) || unseen(c) || (c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
    })
}

/// Characters a person can't see in a contact field that `hides_text` does not name: tag
/// characters, variation selectors, the soft hyphen and the Unicode line and paragraph
/// separators.
fn unseen(c: char) -> bool {
    matches!(
        c,
        '\u{E0000}'..='\u{E007F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{E0100}'..='\u{E01EF}'
            | '\u{00AD}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// A birthday as Apple's card holds it: `YYYY-MM-DD`, a day the month has. A birthday whose
/// year is not known is written with the year 1604, as Apple's own apps write it.
fn birthday(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let number = |r: std::ops::Range<usize>| -> Option<u32> {
        b[r].iter().try_fold(0u32, |n, d| {
            d.is_ascii_digit().then(|| n * 10 + u32::from(d - b'0'))
        })
    };
    let (Some(year), Some(month), Some(day)) = (number(0..4), number(5..7), number(8..10)) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

/// Why a call was refused before anything ran: the code, the field it is about (by the
/// tool's name), and the limit it passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Problem {
    code: &'static str,
    field: &'static str,
    limit: usize,
}

impl Problem {
    fn of(code: &'static str) -> Self {
        Problem {
            code,
            field: "",
            limit: 0,
        }
    }
    fn at(code: &'static str, field: &'static str, limit: usize) -> Self {
        Problem { code, field, limit }
    }
}

/// A field's name in plain words, for the person and the model.
fn label(field: &str) -> String {
    match field {
        "urls" => "web addresses".to_string(),
        "is_company" => "company card".to_string(),
        other => other.replace('_', " "),
    }
}

fn check_fields(fields: &Fields) -> Result<(), Problem> {
    for (name, value, cap) in fields.texts() {
        let Some(value) = value else { continue };
        if value.chars().count() > cap {
            return Err(Problem::at("contact_field_too_long", name, cap));
        }
        if !readable(value, name == "notes") {
            return Err(Problem::at("contact_text_not_allowed", name, 0));
        }
        if name == "birthday" && !value.trim().is_empty() && !birthday(value.trim()) {
            return Err(Problem::at("contact_invalid_birthday", name, 0));
        }
    }
    for (name, _, list) in fields.lists() {
        if list.len() > MAX_ENTRIES {
            return Err(Problem::at("contact_too_many_entries", name, MAX_ENTRIES));
        }
        for entry in &list {
            if entry.value.chars().count() > MAX_VALUE_CHARS {
                return Err(Problem::at("contact_field_too_long", name, MAX_VALUE_CHARS));
            }
            if entry
                .label
                .as_ref()
                .is_some_and(|l| l.chars().count() > MAX_LABEL_CHARS)
            {
                return Err(Problem::at("contact_label_too_long", name, MAX_LABEL_CHARS));
            }
            if !readable(&entry.value, false)
                || entry.label.as_ref().is_some_and(|l| !readable(l, false))
            {
                return Err(Problem::at("contact_text_not_allowed", name, 0));
            }
            if entry.value.trim().is_empty() {
                return Err(Problem::at("contact_empty_entry_value", name, 0));
            }
        }
    }
    Ok(())
}

/// The field names `fields` takes.
const FIELD_NAMES: [&str; 18] = [
    "first_name",
    "middle_name",
    "last_name",
    "company",
    "is_company",
    "birthday",
    "notes",
    "prefix",
    "suffix",
    "nickname",
    "phonetic_first_name",
    "phonetic_last_name",
    "phonetic_company",
    "job_title",
    "department",
    "emails",
    "phones",
    "urls",
];

/// What is wrong with the values in `fields`, by type: text or null, true or false, or a list
/// of `{label, value}` entries.
fn fields_problem(fields: &Value) -> Option<Problem> {
    let Some(fields) = fields.as_object() else {
        return Some(Problem::of("contact_fields_must_be_an_object"));
    };
    for (key, value) in fields {
        let Some(&name) = FIELD_NAMES.iter().find(|n| **n == key.as_str()) else {
            return Some(Problem::of("contact_unknown_field"));
        };
        let wrong = match name {
            "is_company" => !value.is_boolean(),
            "emails" | "phones" | "urls" => match value {
                Value::Null => false,
                Value::Array(entries) => {
                    if entries.iter().any(|e| {
                        !e.is_object()
                            || !e["value"].is_string()
                            || !(e["label"].is_null() || e["label"].is_string())
                            || e.as_object()
                                .is_some_and(|o| o.keys().any(|k| k != "label" && k != "value"))
                    }) {
                        return Some(Problem::at(
                            "contact_entry_must_be_label_and_value",
                            name,
                            0,
                        ));
                    }
                    false
                }
                _ => true,
            },
            _ => !(value.is_string() || value.is_null()),
        };
        if wrong {
            return Some(Problem::at(
                if name == "is_company" {
                    "contact_is_company_must_be_true_or_false"
                } else if matches!(name, "emails" | "phones" | "urls") {
                    "contact_entry_must_be_label_and_value"
                } else {
                    "contact_field_must_be_text_or_null"
                },
                name,
                0,
            ));
        }
    }
    None
}

/// Why arguments that don't parse were refused, as a code that says what each action takes.
fn shape_error(args: &Value) -> Problem {
    let Some(object) = args.as_object() else {
        return Problem::of("invalid_arguments");
    };
    if object
        .keys()
        .any(|k| !["action", "guid", "etag", "fields"].contains(&k.as_str()))
    {
        return Problem::of("contact_unknown_argument");
    }
    let only = |keys: &[&str]| {
        object.keys().all(|k| keys.contains(&k.as_str()))
            && keys.iter().all(|k| object.contains_key(*k))
    };
    let code = match object.get("action").and_then(Value::as_str) {
        Some("create") if !only(&["action", "fields"]) => "contact_create_needs_only_fields",
        Some("update") if !only(&["action", "guid", "etag", "fields"]) => {
            "contact_update_needs_guid_etag_and_fields"
        }
        Some("delete") if !only(&["action", "guid", "etag"]) => {
            "contact_delete_needs_only_guid_and_etag"
        }
        Some("create" | "update") => {
            return fields_problem(&object["fields"])
                .unwrap_or(Problem::of("contact_invalid_guid_or_etag"))
        }
        Some("delete") => "contact_invalid_guid_or_etag",
        _ => "contact_invalid_action",
    };
    Problem::of(code)
}

/// The arguments checked, or what is wrong with them.
fn checked_args(args: &Value) -> Result<ContactsWrite, Problem> {
    // `null` would parse as "left out"; a company flag must say true or false.
    if args["fields"]
        .get("is_company")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(Problem::at(
            "contact_is_company_must_be_true_or_false",
            "is_company",
            0,
        ));
    }
    let op: ContactsWrite = serde_json::from_value(args.clone()).map_err(|_| shape_error(args))?;
    if let Some((guid, etag)) = op.target() {
        if guid.is_empty()
            || guid.len() > contacts::MAX_GUID_BYTES
            || etag.is_empty()
            || etag.len() > MAX_ETAG_BYTES
            || !readable(etag, false)
        {
            return Err(Problem::of("contact_invalid_guid_or_etag"));
        }
    }
    match &op {
        ContactsWrite::Create { fields } => {
            check_fields(fields)?;
            let mut card = blank_card();
            fields.apply(&mut card);
            if apple::blank_card(&card.contact) {
                return Err(Problem::of(BLANK));
            }
        }
        ContactsWrite::Update { fields, .. } => {
            check_fields(fields)?;
            if fields.is_empty() {
                return Err(Problem::of("contact_nothing_to_change"));
            }
        }
        ContactsWrite::Delete { .. } => {}
    }
    Ok(op)
}

/// Checks the arguments before any approval prompt, cache or network use.
pub(crate) fn validate(args: &Value) -> Result<ContactsWrite, &'static str> {
    checked_args(args).map_err(|p| p.code)
}

/// The sentence for a refused argument, naming the field and its limit.
fn argument_step(args: &Value) -> Option<String> {
    let p = checked_args(args).err()?;
    let field = label(p.field);
    Some(match p.code {
        "contact_field_too_long" => format!("{field}: at most {} characters per value.", p.limit),
        "contact_label_too_long" => format!("{field}: at most {} characters per label.", p.limit),
        "contact_too_many_entries" => format!("{field}: at most {} entries.", p.limit),
        "contact_empty_entry_value" => format!("{field}: every entry needs a value that is not blank."),
        "contact_text_not_allowed" => format!("{field} holds a control character, or one that hides or reorders text. Only notes may hold line breaks and tabs."),
        "contact_invalid_birthday" => "birthday is YYYY-MM-DD with a day the month has. If the year is not known, use 1604, as Apple does.".to_string(),
        "contact_entry_must_be_label_and_value" => format!("{field}: each entry is an object such as {{\"label\":\"HOME\",\"value\":\"ada@example.com\"}}, not a plain string; label may be left out or null."),
        "contact_is_company_must_be_true_or_false" => "is_company is true or false; leave it out to keep it as it is.".to_string(),
        "contact_field_must_be_text_or_null" => format!("{field} is text, or null to clear it."),
        "contact_fields_must_be_an_object" => "fields is an object of field names and values.".to_string(),
        "contact_nothing_to_change" => "fields names nothing to change. Name at least one field to set or clear.".to_string(),
        "contact_invalid_action" => "action must be create, update or delete.".to_string(),
        "contact_create_needs_only_fields" => "create takes action and fields only, such as {\"action\":\"create\",\"fields\":{\"first_name\":\"Ada\"}}. The new card's guid is in the reply.".to_string(),
        "contact_update_needs_guid_etag_and_fields" => "update takes action, guid, etag and fields. Take guid and etag from contacts_get, and name in fields only what changes.".to_string(),
        "contact_delete_needs_only_guid_and_etag" => "delete takes action, guid and etag only. Take guid and etag from contacts_get.".to_string(),
        "contact_invalid_guid_or_etag" => "guid and etag are text as contacts_get shows them: guid at most 512 bytes, etag at most 256.".to_string(),
        "contact_unknown_argument" => {
            let unknown: Vec<String> = args
                .as_object()
                .map(|m| {
                    m.keys()
                        .filter(|k| !["action", "guid", "etag", "fields"].contains(&k.as_str()))
                        .take(5)
                        .map(|k| serde_json::json!(k.chars().take(64).collect::<String>()).to_string())
                        .collect()
                })
                .unwrap_or_default();
            format!("{} is not a field of contacts_write; it takes action, guid, etag and fields. Card details such as first_name go inside fields.", unknown.join(", "))
        }
        "contact_unknown_field" => format!("fields takes {}. contacts_get's details are given flat in fields; postal addresses, photos and lists can't be changed here.", FIELD_NAMES.join(", ")),
        _ => return None,
    })
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
    json!({
        "guid": c.guid,
        "display_name": shown(&c.display_name(), MAX_TEXT_CHARS),
        "etag": c.etag,
    })
}

/// `text` as shown for approval: whole up to `cap` characters, then a count of the rest.
fn shown_up_to(text: &str, cap: usize) -> String {
    let total = text.chars().count();
    let mut out = shown(text, cap);
    if total > cap {
        out.push_str(&format!(" ({} more characters)", total - cap));
    }
    out
}

fn entry_text(label: Option<&str>, value: &str) -> String {
    match label {
        Some(label) => format!(
            "{} {}",
            shown_up_to(label, MAX_LABEL_CHARS),
            shown_up_to(value, MAX_VALUE_CHARS)
        ),
        None => shown_up_to(value, MAX_VALUE_CHARS),
    }
}

/// Old entries shown when a list is replaced.
const SHOWN_ENTRIES: usize = 100;

/// Each field the call sets, with the value the cached card holds now (`None` for a create):
/// `{"field", "to"}` or `{"field", "clear": true}`, with `"now"`; for a list, the entries it
/// removes and adds.
fn changes(fields: &Fields, now: Option<&Card>) -> Vec<Value> {
    let mut out = Vec::new();
    let cap = |name: &str| {
        if name == "notes" {
            MAX_NOTES_CHARS
        } else {
            MAX_TEXT_CHARS
        }
    };
    let current = |name: &str| -> Option<String> {
        let card = now?;
        let c = &card.contact;
        match name {
            "first_name" => c.first_name.clone(),
            "middle_name" => c.middle_name.clone(),
            "last_name" => c.last_name.clone(),
            "company" => c.company.clone(),
            "birthday" => c.birthday.clone(),
            "notes" => c.notes.clone(),
            other => DETAILS
                .iter()
                .find(|(n, _)| *n == other)
                .and_then(|(_, key)| card.details.get(key).cloned().flatten()),
        }
    };
    for (name, value, _) in fields.texts() {
        let mut row = match clean(value.as_deref()) {
            Some(to) => json!({"field": name, "to": shown_up_to(&to, cap(name))}),
            None => json!({"field": name, "clear": true}),
        };
        if now.is_some() {
            row["now"] = json!(current(name).map(|v| shown_up_to(&v, cap(name))));
        }
        out.push(row);
    }
    if let Some(is_company) = fields.is_company {
        let mut row = json!({"field": "is_company", "to": is_company});
        if let Some(card) = now {
            row["now"] = json!(card.contact.is_company);
        }
        out.push(row);
    }
    for (name, kind, list) in fields.lists() {
        let mut add: Vec<(Option<String>, String)> = list
            .iter()
            .map(|e| (clean(e.label.as_deref()), e.value.trim().to_string()))
            .collect();
        let mut remove = Vec::new();
        for m in now
            .map(|c| c.contact.methods.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(|m| m.kind == kind)
        {
            match add.iter().position(|(l, v)| *v == m.value && *l == m.label) {
                Some(i) => {
                    add.remove(i);
                }
                None => remove.push(entry_text(m.label.as_deref(), &m.value)),
            }
        }
        let more = remove.len().saturating_sub(SHOWN_ENTRIES);
        remove.truncate(SHOWN_ENTRIES);
        if more > 0 {
            remove.push(format!("({more} more entries)"));
        }
        let add: Vec<String> = add
            .iter()
            .map(|(l, v)| entry_text(l.as_deref(), v))
            .collect();
        out.push(json!({"field": name, "remove": remove, "add": add}));
    }
    out
}

/// The fields of the cached card `contacts_get` shows cut short: the notes, or a kind of
/// entry with more entries or longer text than it shows. A call that replaces one of them
/// would remove what nobody was shown.
fn cut_short(library: &Library, card: &Card) -> Result<Vec<&'static str>, &'static str> {
    let shown_card = contacts::get(library, json!({"guid": card.contact.guid}));
    let (chars, entries) = match &shown_card {
        Ok(v) if v["card_truncated"] != true => return Ok(Vec::new()),
        Ok(v) => (
            v["truncation"]["max_chars_per_text"].as_u64().unwrap_or(0) as usize,
            v["truncation"]["max_entries_per_list"]
                .as_u64()
                .unwrap_or(0) as usize,
        ),
        // Too large to show at all: nothing of it was seen.
        Err(_) => (0, 0),
    };
    let long = |t: &str| t.chars().count() > chars;
    let mut out = Vec::new();
    if card.contact.notes.as_deref().is_some_and(long) {
        out.push("notes");
    }
    for (name, kind) in [("emails", "email"), ("phones", "phone"), ("urls", "url")] {
        let of: Vec<&ContactMethod> = card
            .contact
            .methods
            .iter()
            .filter(|m| m.kind == kind)
            .collect();
        if of.len() > entries
            || of
                .iter()
                .any(|m| long(&m.value) || m.label.as_deref().is_some_and(long))
        {
            out.push(name);
        }
    }
    Ok(out)
}

/// The cached card an update or delete reaches, checked: at the version given, and for an
/// update, not replacing anything `contacts_get` showed cut short.
fn planned(library: &Library, op: &ContactsWrite) -> Result<Option<Card>, &'static str> {
    let Some((guid, etag)) = op.target() else {
        return Ok(None);
    };
    let card = cached(library, guid, etag)?;
    if let ContactsWrite::Update { fields, .. } = op {
        let cut = cut_short(library, &card)?;
        let replaces = |name: &str| match name {
            "notes" => fields.notes.is_some(),
            "emails" => fields.emails.is_some(),
            "phones" => fields.phones.is_some(),
            _ => fields.urls.is_some(),
        };
        if cut.iter().any(|n| replaces(n)) {
            return Err(CUT_SHORT);
        }
    }
    Ok(Some(card))
}

/// What the approval shows and is bound to: the action, the cached card it reaches, and each
/// field it sets with the value it has now. The same arguments over the same cache give the
/// same preview.
pub(crate) fn preview(library: &Library, args: &Value) -> Result<Value, &'static str> {
    let op = validate(args)?;
    let card = planned(library, &op)?;
    let mut value = json!({
        "action": op.action(),
        "destructive": is_destructive(op.action()),
    });
    if let Some(card) = &card {
        value["contact"] = show_card(card);
    }
    match &op {
        ContactsWrite::Create { fields } => {
            value["changes"] = json!(changes(fields, None));
            value["effect"] = json!("adds one new card to Apple iCloud Contacts");
        }
        ContactsWrite::Update { fields, .. } => {
            value["changes"] = json!(changes(fields, card.as_ref()));
            value["effect"] = json!("changes only these fields on the card at Apple, if it is still at this version; the rest of the card stays as it is");
        }
        ContactsWrite::Delete { .. } => {
            value["effect"] = json!("deletes the card from Apple iCloud Contacts on every device; Contacts has no Recently Deleted, so it cannot be undone");
            if let Some(card) = &card {
                value["removes"] = removes(library, card)?;
            }
        }
    }
    Ok(value)
}

/// What a delete takes away, as the cache holds the card: its name and work, and how many
/// entries of each kind.
fn removes(library: &Library, card: &Card) -> Result<Value, &'static str> {
    let c = &card.contact;
    let count = |kind: &str| c.methods.iter().filter(|m| m.kind == kind).count();
    let addresses = library
        .contact_addresses(&c.guid)
        .map_err(|_| "library_unavailable")?
        .len();
    let others = c
        .methods
        .iter()
        .filter(|m| !["email", "phone", "url", "address"].contains(&m.kind.as_str()))
        .count();
    let text = |v: &Option<String>| v.as_deref().map(|t| shown_up_to(t, MAX_TEXT_CHARS));
    let mut value = json!({
        "name": shown(&c.display_name(), MAX_TEXT_CHARS),
        "company": text(&c.company),
        "job_title": text(&card.details.job_title),
        "birthday": text(&c.birthday),
        "emails": count("email"),
        "phones": count("phone"),
        "web_addresses": count("url"),
        "postal_addresses": addresses,
        "other_entries": others,
        "notes_characters": c.notes.as_deref().map_or(0, |n| n.chars().count()),
        "photo": c.photo_url.is_some(),
    });
    if let Some(map) = value.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    Ok(value)
}

/// "1 email", "2 emails".
fn counted(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A preview in plain lines, for the terminal approval: values without quotes, and each
/// change with what the card holds now.
pub(crate) fn plain_preview(preview: &Value) -> String {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => "nothing".to_string(),
        other => other.to_string(),
    };
    let mut lines = vec![format!(
        "Contacts change: {}",
        match preview["action"].as_str() {
            Some("create") => "make a new card",
            Some("update") => "change this card",
            Some("delete") => "delete this card for good",
            _ => "unknown",
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
    let gone = &preview["removes"];
    if gone.is_object() {
        let n = |key: &str| gone[key].as_u64().unwrap_or(0);
        let mut parts = vec![format!("name {}", text(&gone["name"]))];
        for (key, what) in [
            ("company", "company"),
            ("job_title", "job title"),
            ("birthday", "birthday"),
        ] {
            if gone[key].is_string() {
                parts.push(format!("{what} {}", text(&gone[key])));
            }
        }
        for (key, one, many) in [
            ("emails", "email", "emails"),
            ("phones", "phone", "phones"),
            ("web_addresses", "web address", "web addresses"),
            ("postal_addresses", "postal address", "postal addresses"),
            ("other_entries", "other entry", "other entries"),
        ] {
            if n(key) > 0 {
                parts.push(counted(n(key), one, many));
            }
        }
        if n("notes_characters") > 0 {
            parts.push(format!(
                "notes ({})",
                counted(n("notes_characters"), "character", "characters")
            ));
        }
        if gone["photo"] == true {
            parts.push("a photo".to_string());
        }
        lines.push(format!("It deletes: {}", parts.join(", ")));
    }
    for change in preview["changes"].as_array().into_iter().flatten() {
        let field = label(change["field"].as_str().unwrap_or("?"));
        let now = change
            .get("now")
            .map(|n| format!(" (now: {})", text(n)))
            .unwrap_or_default();
        let line = if let Some(add) = change["add"].as_array() {
            let list = |v: &Value| {
                v.as_array()
                    .map(|a| a.iter().map(text).collect::<Vec<_>>().join("; "))
                    .unwrap_or_default()
            };
            let mut parts = Vec::new();
            if change["remove"].as_array().is_some_and(|r| !r.is_empty()) {
                parts.push(format!("remove {}", list(&change["remove"])));
            }
            if !add.is_empty() {
                parts.push(format!("add {}", list(&change["add"])));
            }
            if parts.is_empty() {
                format!("Replace {field}: no change")
            } else {
                format!("Replace {field}: {}", parts.join("; "))
            }
        } else if change["clear"] == true {
            format!("Clear {field}{now}")
        } else {
            format!("Set {field} to {}{now}", text(&change["to"]))
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// The terminal approval's text for stored arguments, as the cache shows them now.
/// A request the cache no longer matches is out of date and is not shown for approval.
pub(crate) fn approval_text(library: &Library, args: &Value) -> Result<String, &'static str> {
    preview(library, args)
        .map(|preview| plain_preview(&preview))
        .map_err(|_| OUT_OF_DATE)
}

/// What the person can do after `error`, a code of this tool, when there is something. `args`
/// are the call's arguments, so a refused field is named with its limit.
pub(crate) fn next_step(error: &str, args: &Value) -> Option<String> {
    if error.starts_with("contact_") {
        if let Some(step) = argument_step(args).filter(|_| validate(args) == Err(error)) {
            return Some(step);
        }
    }
    Some(match error {
        CHANGED => "The card is not at the version given: it changed on another device or since it was read. Nothing was written. Run contacts_refresh, read it again with contacts_get, and ask again with its new etag.",
        UNKNOWN => "The connection ended before Apple answered, so the change may or may not have been made. Run contacts_refresh and look at the card before trying again; a create tried again without that can make a second card.",
        NOT_CACHED => "The card is not in the local cache. Run contacts_refresh, then find it with contacts_search or contacts_get.",
        GONE => "Apple no longer has this card; it may have been deleted on another device. Run contacts_refresh.",
        BLANK => "A new card needs a name, a company, a phone number or an email address.",
        CUT_SHORT => "contacts_get showed this card cut short, so the notes or a list of emails, phones or web addresses it would replace were not seen in full, and nothing was changed. contacts_get cannot show the card whole. Change those fields on an Apple device or on icloud.com, or change only other fields here.",
        NOTHING_SENT => "Apple's address book could not be read, so nothing was sent. Try again later; if it keeps failing, run contacts_refresh to see why.",
        PAUSED => "Apple wants this sign-in approved on one of the user's Apple devices before it lets Contacts be changed. Nothing was sent. Ask the user to approve it there, then try again.",
        REFUSED => "Apple refused the change, so it was not made. Run contacts_refresh and read the card again; if Apple keeps refusing, make the change on an Apple device or on icloud.com.",
        TOO_LARGE => "The address book or one of its cards is larger than this tool reads, so nothing was sent. Make the change on an Apple device or on icloud.com.",
        UNREADABLE => "Apple's copy of this card could not be read as a contact, so nothing was sent. Make the change on an Apple device or on icloud.com.",
        CREATE_REFUSED => "Apple refused the new card because its address book changed while the card was being sent, so nothing was made. Try the create again.",
        OUT_OF_DATE => "This request is out of date: the card in the local cache changed since it was asked, so it can't be approved. Nothing was sent. Ask for the change again.",
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
    /// The read of the book before the change; past it, nothing was sent.
    pub read_timeout: Duration,
    /// The write; past it, the outcome is unknown.
    pub timeout: Duration,
    /// The read back from Apple after the write.
    pub check_timeout: Duration,
}

impl Default for Seam {
    fn default() -> Self {
        Self {
            client: None,
            read: RefreshLimits::default(),
            read_timeout: Duration::from_secs(60),
            timeout: Duration::from_secs(120),
            check_timeout: Duration::from_secs(60),
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
        E::Refused(_) | E::Protocol(_) => REFUSED,
        _ => UNKNOWN,
    }
}

/// A create's error: one from the engine's own read of the book, made before it writes,
/// means nothing was sent. A busy answer can't be told apart from the write's, so it stays
/// unknown.
fn create_error(e: &quince_icloud::Error) -> &'static str {
    use quince_icloud::Error as E;
    match e {
        E::Protocol(why) if why.starts_with("/co/startup") => NOTHING_SENT,
        E::Network(n) if n.url().is_some_and(|u| u.path().ends_with("/co/startup")) => NOTHING_SENT,
        // A new card carries no version: a conflict is Apple's book moving on while it was
        // sent, and Apple made nothing.
        E::Conflict => CREATE_REFUSED,
        other => apple_error(other),
    }
}

fn read_stop(stop: Stop) -> &'static str {
    match stop {
        Stop::Reauth => "login_required",
        Stop::TooLarge | Stop::BookTooLarge | Stop::CardTooLarge => TOO_LARGE,
        Stop::Busy | Stop::Failed => "contacts_write_read_failed_nothing_sent",
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
        planned(&library, &op)?;
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
    // Timed apart: once Apple took the write, a slow or failed read back is done, unverified.
    let result = match send(&client, &root, &op, seam).await {
        Err(error) => Err(error),
        Ok(Sent::Unchanged(value)) => Ok(value),
        Ok(Sent::Written(guid)) => Ok(checked(&client, &root, &guid, &op, seam, &mut after).await),
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

/// What a change did at Apple: nothing was needed, or the write to this card was taken.
enum Sent {
    Unchanged(Value),
    Written(String),
}

/// The read before the change and the write, each under its own time limit: a read that
/// fails or runs out of time sent nothing; a write that runs out of time has an unknown
/// outcome.
async fn send(
    client: &Client,
    root: &str,
    op: &ContactsWrite,
    seam: &Seam,
) -> Result<Sent, &'static str> {
    // Apple's card as it is now; a create reads the book too, so a book that can't be read
    // is said to have sent nothing.
    let (cards, tokens) = tokio::time::timeout(seam.read_timeout, book(client, root, &seam.read))
        .await
        .map_err(|_| NOTHING_SENT)?
        .map_err(read_stop)?;
    tokio::time::timeout(seam.timeout, write(client, root, op, cards, tokens))
        .await
        .map_err(|_| UNKNOWN)?
}

/// The write itself, against the book just read.
async fn write(
    client: &Client,
    root: &str,
    op: &ContactsWrite,
    cards: Vec<Value>,
    tokens: apple::Tokens,
) -> Result<Sent, &'static str> {
    let http = client.http();
    let params = || client.service_params();
    let existing = match op.target() {
        Some((guid, etag)) => {
            let existing = find(&cards, guid).ok_or(GONE)?;
            if existing.get("etag").and_then(Value::as_str) != Some(etag) {
                return Err(CHANGED);
            }
            Some(existing)
        }
        None => None,
    };
    drop(cards);
    match (op, existing) {
        (ContactsWrite::Create { fields }, _) => {
            let guid = apple::new_contact_id();
            let mut card = blank_card();
            fields.apply(&mut card);
            card.contact.guid = guid.clone();
            let ledger = apple::Ledger::holding(tokens);
            apple::create_card_named(http, root, params(), &ledger, &guid, &card)
                .await
                .map_err(|e| create_error(&e))?;
            Ok(Sent::Written(guid))
        }
        (ContactsWrite::Update { guid, etag, fields }, Some(existing)) => {
            let (base, _) = parsed(&existing).ok_or(UNREADABLE)?;
            if fields.differences(&base).is_empty() {
                return Ok(Sent::Unchanged(json!({
                    "outcome": "unchanged",
                    "contact": {"guid": guid, "etag": etag},
                    "checked": "Apple's card, read just now, already holds these values; nothing was sent",
                    "remote_verified": true,
                })));
            }
            let mut card = base.clone();
            fields.apply(&mut card);
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
            Ok(Sent::Written(guid.clone()))
        }
        (ContactsWrite::Delete { guid, .. }, Some(existing)) => {
            // Sent against the version just checked, so Apple refuses it if the card moves
            // on in between. The engine's `delete` would send Apple's newest version instead.
            let ledger = apple::Ledger::holding(tokens);
            apple::merge::delete_card(http, root, params(), &ledger, &existing)
                .await
                .map_err(|e| apple_error(&e))?;
            Ok(Sent::Written(guid.clone()))
        }
        _ => Err(GONE),
    }
}

/// [`check`] under its own time limit.
async fn checked(
    client: &Client,
    root: &str,
    guid: &str,
    op: &ContactsWrite,
    seam: &Seam,
    after: &mut Settle,
) -> Value {
    let fields = match op {
        ContactsWrite::Create { fields } | ContactsWrite::Update { fields, .. } => Some(fields),
        ContactsWrite::Delete { .. } => None,
    };
    let read_back = check(client, root, guid, fields, &seam.read, after);
    match tokio::time::timeout(seam.check_timeout, read_back).await {
        Ok(value) => value,
        Err(_) => unverified(
            guid,
            "the book could not be read back in time to check it; run contacts_refresh",
        ),
    }
}

/// Apple took the write, and it could not be checked.
fn unverified(guid: &str, why: &str) -> Value {
    json!({
        "outcome": "done",
        "contact": {"guid": guid},
        "checked": format!("Apple accepted the write, but {why}"),
        "remote_verified": false,
    })
}

/// The book read back after the write: what it shows of the change, and what the cache keeps.
/// `fields` is `None` for a delete.
async fn check(
    client: &Client,
    root: &str,
    guid: &str,
    fields: Option<&Fields>,
    limits: &RefreshLimits,
    after: &mut Settle,
) -> Value {
    let cards = match book(client, root, limits).await {
        Ok((cards, _)) => cards,
        Err(_) => {
            return unverified(
                guid,
                "the book could not be read back to check it; run contacts_refresh",
            )
        }
    };
    let found = find(&cards, guid);
    drop(cards);
    let Some(fields) = fields else {
        if found.is_some() {
            return unverified(guid, "the book read back still holds the card");
        }
        *after = Settle::Forget(guid.to_string());
        return json!({
            "outcome": "done",
            "contact": {"guid": guid},
            "checked": "the book was read back from Apple and no longer holds the card",
            "remote_verified": true,
        });
    };
    let Some((card, addresses)) = found.as_ref().and_then(parsed) else {
        return unverified(guid, "the book read back does not hold the card");
    };
    let differ = fields.differences(&card);
    let reply = json!({
        "outcome": "done",
        "contact": {"guid": guid, "etag": card.contact.etag, "display_name": shown(&card.contact.display_name(), MAX_TEXT_CHARS)},
        "checked": if differ.is_empty() {
            "the book was read back from Apple and the card holds every field as asked".to_string()
        } else {
            format!("the book was read back from Apple, and these fields read differently than asked (Apple may have reformatted them): {}", differ.join(", "))
        },
        "differences": differ,
        "remote_verified": differ.is_empty(),
    });
    *after = Settle::Store(Box::new(card), addresses);
    reply
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
        (
            "birthday",
            "Birthday as YYYY-MM-DD. If the year is not known, use 1604, as Apple does.",
        ),
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
        json!({"description": "Whether the card is a company's: true or false. Leave it out to keep it; null is refused.", "type": "boolean"}),
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
        "description": "Change the user's Apple iCloud Contacts, one card per call: create (fields), update (guid, etag, fields) or delete (guid, etag). Take guid and etag from contacts_get; the etag is the card's version. An update or delete of a card that changed since, on another device or in the cache, is refused with contact_changed_conflict and nothing is written: run contacts_refresh, read the card again and ask with its new etag. An update changes only the fields it names; null or an empty string clears a field, and a list of emails, phones or URLs replaces that list (each entry is {label, value}; order is not kept). When contacts_get showed the card cut short (card_truncated), an update that replaces its notes or a list of emails, phones or URLs is refused, because part of it was not seen; change those on an Apple device. Postal addresses, photos, contact lists and merging cards are not changed by this tool. Contacts has no Recently Deleted, so a delete is permanent; every delete needs the user's approval on the approval card; every change follows the approval mode and the rules the user set. The reply says done with what was checked, unchanged when Apple already holds the values, or an error; an unknown outcome means run contacts_refresh and look before trying again; a create tried again without that can make a second card.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"enum": ACTIONS},
                "guid": {"description": "Contact GUID from contacts_list, contacts_search or contacts_get. update and delete only.", "type": "string", "minLength": 1, "maxLength": contacts::MAX_GUID_BYTES},
                "etag": {"description": "The card's version, the etag contacts_get shows. update and delete only.", "type": "string", "minLength": 1, "maxLength": MAX_ETAG_BYTES},
                "fields": {
                    "description": "create and update only. A field left out stays as it is; null or an empty string clears it. contacts_get's first_name, middle_name, last_name, company, is_company, birthday and notes have the same names here, and its details (prefix, suffix, nickname, phonetic_*, job_title, department) are given flat in fields. emails and phones are contacts_get's lists of the same name, and urls are its other_methods of kind url. Postal addresses and the other kinds of other_methods can't be changed here.",
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
    fn apple_and_read_failures_say_what_to_do() {
        for code in [
            "contacts_write_paused_approve_on_apple_device",
            "contacts_write_refused_by_apple",
            "contacts_write_book_too_large_nothing_sent",
            "contacts_write_card_unreadable_nothing_sent",
            "contacts_write_request_out_of_date",
            "contact_create_refused_try_again",
        ] {
            assert!(next_step(code, &json!({})).is_some(), "{code}");
        }
    }

    #[test]
    fn only_delete_is_destructive_and_arguments_are_strict() {
        for action in ACTIONS {
            assert_eq!(is_destructive(action), action == "delete");
        }
        assert!(validate(&json!({"action":"delete","guid":"g","etag":"e"})).is_ok());
        assert_eq!(
            validate(&json!({"action":"delete","guid":"g","etag":"e","fields":{}})),
            Err("contact_delete_needs_only_guid_and_etag")
        );
        assert_eq!(
            validate(&json!({"action":"create","fields":{"photo":"x"}})),
            Err("contact_unknown_field")
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
        for good in ["1990-03-14", "2000-12-31", "1604-02-29", "2000-02-29"] {
            assert!(birthday(good), "{good}");
        }
        for bad in [
            "1990-13-01",
            "1990-3-14",
            "14/03/1990",
            "--03-14",
            "1990-00-10",
            "2001-02-31",
            "2001-02-29",
            "1900-02-29",
            "2001-04-31",
        ] {
            assert!(!birthday(bad), "{bad}");
        }
    }
}
