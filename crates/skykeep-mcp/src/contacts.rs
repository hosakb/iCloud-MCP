//! Apple iCloud Contacts, read-only: cached list, search and get, and an explicit refresh
//! that reads Apple's address book into the local cache. Nothing here writes to Apple or
//! sends queued changes. The engine crates own the fetch, the parsing and the cache.
use crate::account::{binding, Binding};
use quince_auth::{Client, Endpoints, Store};
use quince_core::contacts::{Card, ContactMethod, PostalAddress};
use quince_core::Library;
use quince_icloud::contacts::{self as apple, Tokens};
use quince_sync::{library_lock::LibraryLock, pass, queue::ACCOUNT_KEY};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

const SERVICE: &str = "contacts";
const DEFAULT_LIMIT: usize = 25;
pub(crate) const MAX_LIMIT: usize = 100;
pub(crate) const MAX_QUERY_BYTES: usize = 256;
const MAX_GUID_BYTES: usize = 512;
/// A page stops early, with `next_offset`, rather than grow past this.
const MAX_PAGE_BYTES: usize = 262_144;
const MAX_CARD_BYTES: usize = 524_288;
/// Per summary: longer text is cut and the summary says so.
const MAX_SUMMARY_CHARS: usize = 512;
const MAX_SUMMARY_METHODS: usize = 20;
const REFRESH_SECONDS: u64 = 60;

/// What a cached card does not hold. The cache is a projection of Apple's card, not a copy.
const NOT_INCLUDED: [&str; 5] = [
    "photo image data and the photo's location; only whether a photo exists",
    "the original vCard; this is not a lossless vCard export",
    "custom fields and card properties the cache does not keep: anything beyond names, company, birthday, notes, name and work details, phones, emails, URLs, postal addresses, related names, dates, instant messaging and social profiles",
    "instant messaging and social profile entries keep one value and one label each",
    "labels are Apple's stored label strings, untranslated; custom labels are kept as typed",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    guid: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

fn parse<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, &'static str> {
    serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")
}

fn page_limit(limit: Option<usize>) -> Result<usize, &'static str> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
        Some(_) => Err("invalid_arguments"),
    }
}

fn search_args(args: &Value) -> Result<Search, &'static str> {
    let search: Search = parse(args)?;
    page_limit(search.limit)?;
    if search.query.len() > MAX_QUERY_BYTES {
        return Err("query_too_long");
    }
    if search.query.trim().is_empty() {
        return Err("query_required");
    }
    Ok(search)
}

fn get_args(args: &Value) -> Result<Get, &'static str> {
    let get: Get = parse(args)?;
    if get.guid.is_empty() || get.guid.len() > MAX_GUID_BYTES {
        return Err("invalid_arguments");
    }
    Ok(get)
}

/// Checks a Contacts tool's arguments before any approval prompt or cache access.
pub(crate) fn validate(name: &str, args: &Value) -> Result<(), &'static str> {
    match name {
        "contacts_list" => page_limit(parse::<Page>(args)?.limit).map(|_| ()),
        "contacts_search" => search_args(args).map(|_| ()),
        "contacts_get" => get_args(args).map(|_| ()),
        "contacts_refresh" => parse::<Empty>(args).map(|_| ()),
        _ => Ok(()),
    }
}

/// How current the cache is. Apple error text stays in the library; only its presence shows.
fn cache_state(library: &Library) -> Result<Value, &'static str> {
    let health = library.sync_health().map_err(|_| "library_unavailable")?;
    let mine = health.iter().find(|h| h.service == SERVICE);
    let at = mine.and_then(|h| h.last_synced_at);
    Ok(json!({
        "refreshed": at.is_some(),
        "last_refreshed_at": at,
        "last_refresh_failed": mine.is_some_and(|h| h.last_error.is_some()),
        "refresh": "only by contacts_refresh; there is no background Contacts refresh",
    }))
}

/// Display order: folded last name then first name (else what the card shows), with digit
/// runs compared as numbers, then GUID so equal names keep one order.
fn ordered(mut cards: Vec<Card>) -> Vec<Card> {
    let key = |card: &Card| {
        let c = &card.contact;
        let name = [c.last_name.as_deref(), c.first_name.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        if name.trim().is_empty() {
            c.display_name()
        } else {
            name
        }
    };
    let mut keyed: Vec<(String, Card)> = cards.drain(..).map(|c| (key(&c), c)).collect();
    keyed.sort_by(|(a, x), (b, y)| {
        quince_core::fold::natural(a, b).then_with(|| x.contact.guid.cmp(&y.contact.guid))
    });
    keyed.into_iter().map(|(_, c)| c).collect()
}

fn cut(text: Option<&str>, cut_any: &mut bool) -> Option<String> {
    text.map(|t| {
        if t.chars().count() > MAX_SUMMARY_CHARS {
            *cut_any = true;
            t.chars().take(MAX_SUMMARY_CHARS).collect()
        } else {
            t.to_string()
        }
    })
}

fn summary(card: &Card) -> Value {
    let c = &card.contact;
    let mut cut_any = false;
    let mut of_kind = |kind: &str| {
        let all: Vec<&ContactMethod> = c.methods.iter().filter(|m| m.kind == kind).collect();
        if all.len() > MAX_SUMMARY_METHODS {
            cut_any = true;
        }
        let shown: Vec<Value> = all
            .iter()
            .take(MAX_SUMMARY_METHODS)
            .map(|m| {
                json!({"label":cut(m.label.as_deref(), &mut cut_any),"value":cut(Some(&m.value), &mut cut_any)})
            })
            .collect();
        (shown, all.len())
    };
    let (emails, emails_total) = of_kind("email");
    let (phones, phones_total) = of_kind("phone");
    json!({
        "guid": c.guid,
        "display_name": cut(Some(&c.display_name()), &mut cut_any),
        "first_name": cut(c.first_name.as_deref(), &mut cut_any),
        "middle_name": cut(c.middle_name.as_deref(), &mut cut_any),
        "last_name": cut(c.last_name.as_deref(), &mut cut_any),
        "company": cut(c.company.as_deref(), &mut cut_any),
        "is_company": c.is_company,
        "emails": emails,
        "emails_total": emails_total,
        "phones": phones,
        "phones_total": phones_total,
        "has_photo": c.photo_url.is_some(),
        "summary_truncated": cut_any,
    })
}

fn paged(
    library: &Library,
    cards: Vec<Card>,
    offset: usize,
    limit: usize,
) -> Result<Value, &'static str> {
    let total = cards.len();
    let mut items = Vec::new();
    let mut used = 0usize;
    let mut shortened = false;
    for card in cards.iter().skip(offset).take(limit) {
        let item = summary(card);
        let size = item.to_string().len();
        if !items.is_empty() && used.saturating_add(size) > MAX_PAGE_BYTES {
            shortened = true;
            break;
        }
        used = used.saturating_add(size);
        items.push(item);
    }
    let next = offset.saturating_add(items.len());
    Ok(json!({
        "source": "local_cache",
        "items": items,
        "total": total,
        "offset": offset,
        "limit": limit,
        "next_offset": (next < total).then_some(next),
        "page_shortened_to_fit": shortened,
        "complete_representation": false,
        "cache": cache_state(library)?,
        "remote_verified": false,
    }))
}

pub(crate) fn list(library: &Library, args: Value) -> Result<Value, &'static str> {
    let args: Page = parse(&args)?;
    let limit = page_limit(args.limit)?;
    let cards = ordered(library.cards(None).map_err(|_| "library_unavailable")?);
    paged(library, cards, args.offset, limit)
}

pub(crate) fn search(library: &Library, args: Value) -> Result<Value, &'static str> {
    let args = search_args(&args)?;
    let limit = page_limit(args.limit)?;
    let cards = ordered(
        library
            .cards(Some(&args.query))
            .map_err(|_| "search_unavailable")?,
    );
    paged(library, cards, args.offset, limit)
}

fn method_view(m: &ContactMethod) -> Value {
    json!({"label":m.label,"value":m.value})
}

fn address_view(a: &PostalAddress) -> Value {
    json!({
        "label": a.label,
        "street": a.street,
        "sub_locality": a.sub_locality,
        "city": a.city,
        "sub_administrative_area": a.sub_administrative_area,
        "state": a.state,
        "postal_code": a.postal_code,
        "country": a.country,
        "country_code": a.country_code,
    })
}

pub(crate) fn get(library: &Library, args: Value) -> Result<Value, &'static str> {
    let args = get_args(&args)?;
    let card = library
        .cards_by_guid(std::slice::from_ref(&args.guid))
        .map_err(|_| "library_unavailable")?
        .into_iter()
        .find(|c| c.contact.guid == args.guid)
        .ok_or("not_found")?;
    let c = &card.contact;
    let d = &card.details;
    let addresses = library
        .contact_addresses(&c.guid)
        .map_err(|_| "library_unavailable")?;
    let lists: Vec<Value> = apple::stored_lists(library)
        .into_iter()
        .filter(|l| l.members.contains(&c.guid))
        .map(|l| json!({"guid":l.guid,"name":l.name}))
        .collect();
    let kind = |k: &str| -> Vec<Value> {
        c.methods
            .iter()
            .filter(|m| m.kind == k)
            .map(method_view)
            .collect()
    };
    let others: Vec<Value> = c
        .methods
        .iter()
        .filter(|m| !["email", "phone", "address"].contains(&m.kind.as_str()))
        .map(|m| json!({"kind":m.kind,"label":m.label,"value":m.value}))
        .collect();
    let value = json!({
        "source": "local_cache",
        "contact": {
            "guid": c.guid,
            "display_name": c.display_name(),
            "first_name": c.first_name,
            "middle_name": c.middle_name,
            "last_name": c.last_name,
            "company": c.company,
            "is_company": c.is_company,
            "birthday": c.birthday,
            "notes": c.notes,
            "etag": c.etag,
            "details": {
                "prefix": d.prefix,
                "suffix": d.suffix,
                "nickname": d.nickname,
                "phonetic_first_name": d.phonetic_first_name,
                "phonetic_last_name": d.phonetic_last_name,
                "phonetic_company": d.phonetic_company,
                "job_title": d.job_title,
                "department": d.department,
            },
            "emails": kind("email"),
            "phones": kind("phone"),
            "postal_addresses": addresses.iter().map(address_view).collect::<Vec<_>>(),
            "other_methods": others,
            "lists": lists,
            "photo": {"present": c.photo_url.is_some(), "image_included": false},
        },
        "complete_representation": false,
        "not_included": NOT_INCLUDED,
        "cache": cache_state(library)?,
        "remote_verified": false,
    });
    if value.to_string().len() > MAX_CARD_BYTES {
        return Err("contact_too_large");
    }
    Ok(value)
}

/// Refuses before any keyring, session or network use: an unapproved read under
/// `reads_and_writes`, or a directory not yet bound to an account. Touches no queued write.
fn gate(dir: &Path, approved: bool) -> Result<(), &'static str> {
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if crate::policy(&library, dir)?.is_some_and(|(p, _)| p.mode == "reads_and_writes") && !approved
    {
        return Err("read_approval_required");
    }
    if library
        .state(ACCOUNT_KEY)
        .map_err(|_| "library_unavailable")?
        .is_none()
    {
        return Err("login_required");
    }
    Ok(())
}

/// Production refresh: the directory's own keyring session and Apple's endpoints only.
/// Caller holds the private directory's exclusive operation lock.
pub(crate) async fn refresh(dir: &Path, approved: bool) -> Result<Value, &'static str> {
    gate(dir, approved)?;
    let store = Store::isolated_keyring(dir).map_err(|_| "secure_storage_unavailable")?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    let client = Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")?;
    timed(dir, client).await
}

/// Test seam with synthetic endpoints, never approved for `reads_and_writes`.
/// Caller holds the private directory's exclusive operation lock.
pub async fn refresh_with_client(dir: &Path, client: Client) -> Result<Value, &'static str> {
    gate(dir, false)?;
    timed(dir, client).await
}

async fn timed(dir: &Path, client: Client) -> Result<Value, &'static str> {
    // The store is one synchronous transaction after the network half, so a timeout
    // leaves either the old cache or the new one.
    tokio::time::timeout(
        std::time::Duration::from_secs(REFRESH_SECONDS),
        read_book(dir, client),
    )
    .await
    .map_err(|_| "contacts_refresh_timeout_cache_kept")?
}

async fn read_book(dir: &Path, mut client: Client) -> Result<Value, &'static str> {
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
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if binding(&library, dsid)? != Binding::Same {
        return Err("account_mismatch_use_new_state_directory");
    }
    let root = client
        .session()
        .service_url("contacts")
        .ok_or("contacts_service_unavailable")?;
    let data = LibraryLock::new(library, Some(dir.to_owned()));
    // The pass stores only for the account its parameters ask as, and only while the
    // library still names that account.
    let ctx = pass::Ctx::new(
        client.http().clone(),
        client.service_params(),
        None,
        Vec::new(),
        &pass::NoHooks,
    );
    let since = pass::token(&data, SERVICE)
        .await
        .as_deref()
        .and_then(Tokens::from_stored);
    let reauth = std::sync::atomic::AtomicBool::new(false);
    let lock = tokio::sync::Mutex::new(());
    let report = pass::driven(SERVICE, &ctx, &data, &lock, async {
        let fetched = apple::pass(&ctx.http, &root, ctx.params.clone(), since).await;
        if matches!(fetched, Err(quince_icloud::Error::ReauthRequired)) {
            reauth.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // Queued Contacts changes are neither sent nor laid over what Apple returned.
        pass::store(
            SERVICE,
            &data,
            &ctx,
            fetched,
            |library, found| match found {
                apple::Pass::Read(book, tokens) => {
                    apple::apply(library, book)?;
                    library.mark_synced(SERVICE, Some(&tokens.stored()))?;
                    library.mark_full_sync(SERVICE)?;
                    Ok("full".to_string())
                }
                apple::Pass::Unchanged(tokens) => {
                    library.mark_synced(SERVICE, Some(&tokens.stored()))?;
                    Ok("unchanged".to_string())
                }
            },
        )
        .await
    })
    .await;
    match report.outcome {
        pass::Outcome::Ok => {}
        pass::Outcome::Paused => return Err("contacts_paused_approve_on_apple_device"),
        _ if reauth.load(std::sync::atomic::Ordering::SeqCst) => return Err("login_required"),
        _ => return Err("contacts_refresh_failed_cache_kept"),
    }
    let library = data.lock().await;
    let cached = library.contact_count().map_err(|_| "library_unavailable")?;
    let queued = library
        .outbox_pending(SERVICE)
        .map_err(|_| "library_unavailable")?
        .len();
    Ok(json!({
        "service": SERVICE,
        "outcome": "ok",
        "read": report.detail.as_deref().unwrap_or("full"),
        "cached_contacts": cached,
        "last_refreshed_at": report.last_synced_at,
        "queued_contact_changes": queued,
        "queued_changes_sent": 0,
        "remote_verified": false,
        "verification": "Apple's address book was read into the local cache; nothing was sent to Apple",
    }))
}
