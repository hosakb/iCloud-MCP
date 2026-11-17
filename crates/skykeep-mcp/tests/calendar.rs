//! Apple iCloud Calendar, read-only: cached calendars, events over a bounded range, search,
//! one event, and an explicit refresh that reads Apple's calendar into the cache.
//! Every fixture is synthetic and every remote is a loopback mock.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::Server;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DSID: &str = "1234567890";

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn date(y: i64, m: i64, d: i64, h: i64, min: i64) -> Value {
    json!([y * 10_000 + m * 100 + d, y, m, d, h, min, h * 60 + min])
}

fn timed(guid: &str, cal: &str, title: &str, tz: Option<&str>, start: Value, end: Value) -> Value {
    let mut e = json!({"guid":guid,"pGuid":cal,"title":title,"startDate":start,"endDate":end,"etag":format!("etag-{guid}")});
    if let Some(tz) = tz {
        e["tz"] = json!(tz);
    }
    e
}

fn all_day(guid: &str, title: &str, start: (i64, i64, i64), end: (i64, i64, i64)) -> Value {
    json!({"guid":guid,"pGuid":"cal-work","title":title,"allDay":true,
        "startDate":date(start.0,start.1,start.2,0,0),"endDate":date(end.0,end.1,end.2,0,0)})
}

fn collections() -> Value {
    json!([
        {"guid":"cal-work","title":"Work","color":"#34aadc","order":1,"isDefault":true,"ctag":"c1"},
        {"guid":"cal-shared","title":"Team (shared)","order":2,"readOnly":true,"enabled":false,"ctag":"c2"},
        // Reminder lists come through the same endpoint and are not calendars.
        {"guid":"list-tasks","title":"Tasks","supportedType":"Task"}
    ])
}

/// Apple-shaped events covering the zone, all-day, floating and repeat cases.
fn apple_events() -> Value {
    json!([
        // Berlin's clocks go forward at 02:00 on 2026-03-29: 09:30 there is 07:30 UTC.
        timed(
            "E-DST",
            "cal-work",
            "After the change",
            Some("Europe/Berlin"),
            date(2026, 3, 29, 9, 30),
            date(2026, 3, 29, 10, 30)
        ),
        // The evening before, still on winter time: 23:30 there is 22:30 UTC.
        timed(
            "E-EVE",
            "cal-work",
            "Late call",
            Some("Europe/Berlin"),
            date(2026, 3, 28, 23, 30),
            date(2026, 3, 28, 23, 59)
        ),
        // New York repeats 01:00-02:00 on 2026-11-01; Calendar takes the first, daylight, one.
        timed(
            "E-FOLD",
            "cal-shared",
            "Night shift",
            Some("America/New_York"),
            date(2026, 11, 1, 1, 30),
            date(2026, 11, 1, 2, 30)
        ),
        // An Outlook zone name, read as the IANA zone it is.
        timed(
            "E-WIN",
            "cal-work",
            "Outlook invite",
            Some("W. Europe Standard Time"),
            date(2026, 7, 1, 14, 0),
            date(2026, 7, 1, 15, 0)
        ),
        // A zone no database knows is read as UTC, and the event says so.
        timed(
            "E-ODD",
            "cal-work",
            "Odd zone",
            Some("Mars/Olympus"),
            date(2026, 7, 2, 8, 0),
            date(2026, 7, 2, 9, 0)
        ),
        // A day on the calendar, and three days.
        all_day("A-1", "Holiday", (2026, 10, 5), (2026, 10, 6)),
        all_day("A-3", "Trip", (2026, 10, 10), (2026, 10, 13)),
        // Floating: 09:00 wherever it is looked at.
        timed(
            "F-1",
            "cal-work",
            "Gym",
            None,
            date(2026, 10, 5, 9, 0),
            date(2026, 10, 5, 10, 0)
        ),
        // A weekly series, Apple sending each occurrence. The middle one was moved to 11:00
        // (an exception); its id keeps the start it had in the series.
        timed(
            "WEEKLY__20261102T090000Z",
            "cal-work",
            "Standup",
            Some("UTC"),
            date(2026, 11, 2, 9, 0),
            date(2026, 11, 2, 9, 15)
        ),
        timed(
            "WEEKLY__20261109T090000Z",
            "cal-work",
            "Standup (moved)",
            Some("UTC"),
            date(2026, 11, 9, 11, 0),
            date(2026, 11, 9, 11, 15)
        ),
        timed(
            "WEEKLY__20261116T090000Z",
            "cal-work",
            "Standup",
            Some("UTC"),
            date(2026, 11, 16, 9, 0),
            date(2026, 11, 16, 9, 15)
        ),
    ])
}

fn apple_body() -> Value {
    json!({"Collection": collections(), "Event": apple_events()})
}

fn store_body(library: &mut quince_core::Library, body: &Value) {
    let snapshot = quince_icloud::calendar::parse_snapshot(body);
    quince_icloud::sync::apply_calendar(library, snapshot).unwrap();
}

/// A calendar of another account and a subscribed feed: never Apple iCloud's.
fn add_foreign_calendars(library: &mut quince_core::Library) {
    let mut all = library.calendars().unwrap();
    let dav = format!("{}acct/home", quince_core::dav::PREFIX);
    let feed = format!("{}school", quince_icloud::calendar_feeds::PREFIX);
    for guid in [&dav, &feed] {
        all.push(quince_core::calendar::Calendar {
            guid: guid.clone(),
            title: "Not iCloud".into(),
            ..Default::default()
        });
    }
    library.put_calendars(&all).unwrap();
    library
        .put_events(&[
            quince_core::calendar::Event {
                guid: "DAV-1".into(),
                calendar_guid: dav,
                title: "Nextcloud meeting".into(),
                starts_at: 1_790_000_000,
                ends_at: 1_790_003_600,
                timezone: Some("UTC".into()),
                ..Default::default()
            },
            quince_core::calendar::Event {
                guid: "FEED-1".into(),
                calendar_guid: feed,
                title: "School play".into(),
                starts_at: 1_790_000_000,
                ends_at: 1_790_003_600,
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ])
        .unwrap();
}

/// A private directory bound to the synthetic account, holding the synthetic calendar.
fn seeded(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    store_body(&mut library, &apple_body());
    add_foreign_calendars(&mut library);
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    (dir, server)
}

async fn init(server: &mut Server, forms: bool) {
    let capabilities = if forms {
        json!({"elicitation":{"form":{}}})
    } else {
        json!({})
    };
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":capabilities}}))
        .await
        .unwrap();
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn call(server: &mut Server, id: i64, name: &str, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap()
}

async fn ok(server: &mut Server, name: &str, args: Value) -> Value {
    let reply = call(server, 7, name, args).await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    text(&reply)
}

async fn error(server: &mut Server, name: &str, args: Value) -> String {
    let reply = call(server, 7, name, args).await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    text(&reply)["error"].as_str().unwrap().to_string()
}

fn guids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["guid"].as_str().unwrap().to_string())
        .collect()
}

fn item<'a>(page: &'a Value, guid: &str) -> &'a Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["guid"] == guid)
        .unwrap_or_else(|| panic!("{guid} missing from {page}"))
}

async fn events(server: &mut Server, from: &str, to: &str, tz: &str) -> Value {
    ok(
        server,
        "calendar_events",
        json!({"from":from,"to":to,"timezone":tz}),
    )
    .await
}

const NAMES: [&str; 5] = [
    "calendar_calendars",
    "calendar_events",
    "calendar_search",
    "calendar_get",
    "calendar_refresh",
];

#[tokio::test]
async fn tools_list_names_apple_icloud_calendar_with_strict_read_only_schemas() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let init = server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Apple iCloud Calendar"),
        "{instructions}"
    );
    assert!(instructions.contains("calendar_refresh"), "{instructions}");
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let calendar: Vec<&Value> = tools
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with("calendar"))
        .collect();
    let names: Vec<&str> = calendar
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, NAMES);
    for tool in &calendar {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("Apple iCloud Calendar"),
            "{name}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        let hints = &tool["annotations"];
        assert_eq!(hints["destructiveHint"], false, "{name}");
        if name == "calendar_refresh" {
            assert_eq!(hints["readOnlyHint"], false, "refresh replaces the cache");
            assert_eq!(hints["openWorldHint"], true, "refresh reads from Apple");
            assert!(tool["inputSchema"].get("required").is_none());
        } else {
            assert_eq!(hints["readOnlyHint"], true, "{name}");
            assert_eq!(hints["openWorldHint"], false, "{name}");
        }
        assert!(!name.contains("write") && !name.contains("delete"));
    }
    let range = calendar[1];
    assert_eq!(
        range["inputSchema"]["required"],
        json!(["from", "to", "timezone"])
    );
    assert_eq!(range["inputSchema"]["properties"]["limit"]["maximum"], 100);
    assert_eq!(calendar[2]["inputSchema"]["required"], json!(["query"]));
    assert_eq!(calendar[3]["inputSchema"]["required"], json!(["guid"]));
    // Nothing in this slice creates, edits, deletes or answers invitations.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for word in ["invite", "attendee", "rsvp", "caldav", "nextcloud"] {
            assert!(!name.contains(word), "{name}");
        }
    }
}

#[tokio::test]
async fn calendars_lists_only_apple_icloud_calendars_with_their_flags() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let page = ok(&mut server, "calendar_calendars", json!({})).await;
    assert_eq!(guids(&page), ["cal-work", "cal-shared"]);
    let work = item(&page, "cal-work");
    assert_eq!(work["title"], "Work");
    assert_eq!(work["is_default"], true);
    assert_eq!(work["read_only"], false);
    let shared = item(&page, "cal-shared");
    assert_eq!(shared["read_only"], true);
    assert_eq!(shared["shown_on_icloud_web"], false);
    assert_eq!(page["source"], "local_cache");
    assert_eq!(page["remote_verified"], false);
    assert_eq!(page["total"], 2);
    // Paged.
    let first = ok(&mut server, "calendar_calendars", json!({"limit":1})).await;
    assert_eq!(guids(&first), ["cal-work"]);
    assert_eq!(first["next_offset"], 1);
    let second = ok(
        &mut server,
        "calendar_calendars",
        json!({"limit":1,"offset":1}),
    )
    .await;
    assert_eq!(guids(&second), ["cal-shared"]);
    assert!(second["next_offset"].is_null());
    assert_eq!(
        error(&mut server, "calendar_calendars", json!({"limit":0})).await,
        "invalid_arguments"
    );
    assert_eq!(
        error(&mut server, "calendar_calendars", json!({"limit":101})).await,
        "invalid_arguments"
    );
}

#[tokio::test]
async fn timed_events_keep_their_zone_across_daylight_saving_changes() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    // The day Berlin's clocks change, as Berlin sees it.
    let day = events(&mut server, "2026-03-29", "2026-03-30", "Europe/Berlin").await;
    assert_eq!(guids(&day), ["E-DST"]);
    let e = item(&day, "E-DST");
    assert_eq!(e["all_day"], false);
    assert_eq!(e["floating"], false);
    assert_eq!(e["timezone"], "Europe/Berlin");
    assert_eq!(e["start"]["utc"], "2026-03-29T07:30:00Z");
    assert_eq!(e["start"]["in_event_zone"], "2026-03-29T09:30:00+02:00");
    assert_eq!(e["start"]["in_requested_zone"], "2026-03-29T09:30:00+02:00");
    assert_eq!(e["end"]["utc"], "2026-03-29T08:30:00Z");
    assert_eq!(day["range"]["timezone"], "Europe/Berlin");
    assert_eq!(day["range"]["to_is_exclusive"], true);
    // The evening before is on winter time, and falls on the 28th in Berlin.
    let eve = events(&mut server, "2026-03-28", "2026-03-29", "Europe/Berlin").await;
    assert_eq!(guids(&eve), ["E-EVE"]);
    assert_eq!(
        item(&eve, "E-EVE")["start"]["in_event_zone"],
        "2026-03-28T23:30:00+01:00"
    );
    // The same two events seen from New York (already on daylight time): both on the 28th
    // until 20:00 there, then E-DST at 03:30 on the 29th.
    let ny = events(&mut server, "2026-03-28", "2026-03-29", "America/New_York").await;
    assert_eq!(guids(&ny), ["E-EVE"]);
    assert_eq!(
        item(&ny, "E-EVE")["start"]["in_requested_zone"],
        "2026-03-28T18:30:00-04:00"
    );
    let ny = events(&mut server, "2026-03-29", "2026-03-30", "America/New_York").await;
    assert_eq!(guids(&ny), ["E-DST"]);
    // The repeated hour in New York is read as its first, daylight-time, occurrence.
    let fold = events(&mut server, "2026-11-01", "2026-11-02", "America/New_York").await;
    let e = item(&fold, "E-FOLD");
    assert_eq!(e["start"]["in_event_zone"], "2026-11-01T01:30:00-04:00");
    assert_eq!(e["start"]["utc"], "2026-11-01T05:30:00Z");
    // A Windows zone name is reported by its IANA name.
    let july = events(&mut server, "2026-07-01", "2026-07-03", "UTC").await;
    let win = item(&july, "E-WIN");
    assert_eq!(win["timezone"], "Europe/Berlin");
    assert_eq!(win["start"]["in_event_zone"], "2026-07-01T14:00:00+02:00");
    assert_eq!(win["timezone_resolved"], true);
    // An unknown zone was read as UTC, and the event says so rather than guessing.
    let odd = item(&july, "E-ODD");
    assert_eq!(odd["timezone"], "Mars/Olympus");
    assert_eq!(odd["timezone_resolved"], false);
    assert_eq!(odd["start"]["utc"], "2026-07-02T08:00:00Z");
    assert_eq!(odd["start"]["in_event_zone"], "2026-07-02T08:00:00+00:00");
}

#[tokio::test]
async fn all_day_and_floating_events_stay_on_their_day_in_every_zone() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    for zone in [
        "Pacific/Kiritimati",
        "Pacific/Pago_Pago",
        "UTC",
        "Asia/Kolkata",
    ] {
        let day = events(&mut server, "2026-10-05", "2026-10-06", zone).await;
        assert_eq!(guids(&day), ["A-1", "F-1"], "{zone}");
        let holiday = item(&day, "A-1");
        assert_eq!(holiday["all_day"], true);
        assert_eq!(holiday["floating"], true);
        assert_eq!(holiday["start"], json!({"date":"2026-10-05"}), "{zone}");
        assert_eq!(holiday["end"], json!({"date":"2026-10-06"}), "{zone}");
        let gym = item(&day, "F-1");
        assert_eq!(gym["all_day"], false);
        assert_eq!(gym["floating"], true);
        assert!(gym["timezone"].is_null());
        assert_eq!(gym["start"], json!({"local":"2026-10-05T09:00:00"}));
        assert_eq!(gym["end"], json!({"local":"2026-10-05T10:00:00"}));
        // Not the day before or after, wherever the reader is.
        let before = events(&mut server, "2026-10-04", "2026-10-05", zone).await;
        assert!(guids(&before).is_empty(), "{zone}: {before}");
        let after = events(&mut server, "2026-10-06", "2026-10-07", zone).await;
        assert!(guids(&after).is_empty(), "{zone}: {after}");
    }
    // A three-day all-day event shows on each of its days, with an exclusive end.
    let middle = events(&mut server, "2026-10-11", "2026-10-12", "UTC").await;
    assert_eq!(guids(&middle), ["A-3"]);
    assert_eq!(item(&middle, "A-3")["end"], json!({"date":"2026-10-13"}));
    let gone = events(&mut server, "2026-10-13", "2026-10-14", "UTC").await;
    assert!(guids(&gone).is_empty());
}

#[tokio::test]
async fn repeating_events_name_their_series_and_say_which_occurrence_moved() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let month = events(&mut server, "2026-11-02", "2026-11-17", "UTC").await;
    assert_eq!(
        guids(&month),
        [
            "WEEKLY__20261102T090000Z",
            "WEEKLY__20261109T090000Z",
            "WEEKLY__20261116T090000Z"
        ]
    );
    let first = item(&month, "WEEKLY__20261102T090000Z");
    assert_eq!(
        first["occurrence"],
        json!({"series_guid":"WEEKLY","original_start_utc":"2026-11-02T09:00:00Z","moved":false})
    );
    let moved = item(&month, "WEEKLY__20261109T090000Z");
    assert_eq!(moved["occurrence"]["moved"], true);
    assert_eq!(moved["start"]["utc"], "2026-11-09T11:00:00Z");
    assert_eq!(moved["title"], "Standup (moved)");
    // A one-off event is not an occurrence.
    let day = events(&mut server, "2026-03-29", "2026-03-30", "UTC").await;
    assert!(item(&day, "E-DST")["occurrence"].is_null());
}

#[tokio::test]
async fn ranges_are_bounded_validated_and_paged_in_a_stable_order() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    for (args, code) in [
        (
            json!({"from":"2026-01-01","to":"2026-01-01","timezone":"UTC"}),
            "invalid_range",
        ),
        (
            json!({"from":"2026-02-01","to":"2026-01-01","timezone":"UTC"}),
            "invalid_range",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-04-05","timezone":"UTC"}),
            "range_too_wide",
        ),
        (
            json!({"from":"2026-02-30","to":"2026-03-01","timezone":"UTC"}),
            "invalid_date",
        ),
        (
            json!({"from":"2026-1-01","to":"2026-03-01","timezone":"UTC"}),
            "invalid_date",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-02-01","timezone":"Nowhere/Land"}),
            "unknown_timezone",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-02-01"}),
            "invalid_arguments",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-02-01","timezone":"UTC","calendar_guid":"cal-none"}),
            "calendar_not_found",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-02-01","timezone":"UTC","calendar_guid":"quince:dav:acct/home"}),
            "calendar_not_found",
        ),
        (
            json!({"from":"2026-01-01","to":"2026-02-01","timezone":"UTC","extra":1}),
            "invalid_arguments",
        ),
    ] {
        assert_eq!(
            error(&mut server, "calendar_events", args.clone()).await,
            code,
            "{args}"
        );
    }
    // 93 days is the widest range.
    ok(
        &mut server,
        "calendar_events",
        json!({"from":"2026-01-01","to":"2026-04-04","timezone":"UTC"}),
    )
    .await;
    // Pages walk the whole range once, in start order then id.
    let all = events(&mut server, "2026-10-01", "2026-12-31", "UTC").await;
    assert_eq!(
        guids(&all),
        [
            "A-1",
            "F-1",
            "A-3",
            "E-FOLD",
            "WEEKLY__20261102T090000Z",
            "WEEKLY__20261109T090000Z",
            "WEEKLY__20261116T090000Z"
        ]
    );
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = ok(
            &mut server,
            "calendar_events",
            json!({"from":"2026-10-01","to":"2026-12-31","timezone":"UTC","limit":2,"offset":offset}),
        )
        .await;
        assert_eq!(page["total"], 7);
        seen.extend(guids(&page));
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(seen, guids(&all));
    // One calendar only.
    let shared = ok(
        &mut server,
        "calendar_events",
        json!({"from":"2026-10-01","to":"2026-12-31","timezone":"UTC","calendar_guid":"cal-shared"}),
    )
    .await;
    assert_eq!(guids(&shared), ["E-FOLD"]);
    // Calendars of other accounts and subscribed feeds never show.
    let foreign = events(&mut server, "2026-09-20", "2026-09-23", "UTC").await;
    assert!(guids(&foreign).is_empty(), "{foreign}");
}

#[tokio::test]
async fn search_and_get_read_the_cached_event_and_say_what_is_not_kept() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let found = ok(&mut server, "calendar_search", json!({"query":"standup"})).await;
    assert_eq!(found["total"], 3);
    assert_eq!(found["may_be_incomplete"], false);
    assert_eq!(found["search_keeps_at_most"], 300);
    assert_eq!(
        error(&mut server, "calendar_search", json!({"query":"  "})).await,
        "query_required"
    );
    assert_eq!(
        error(
            &mut server,
            "calendar_search",
            json!({"query":"x".repeat(257)})
        )
        .await,
        "query_too_long"
    );
    // Events of other accounts are not searched.
    let foreign = ok(&mut server, "calendar_search", json!({"query":"nextcloud"})).await;
    assert_eq!(foreign["total"], 0);

    let got = ok(&mut server, "calendar_get", json!({"guid":"E-DST"})).await;
    let e = &got["event"];
    assert_eq!(e["title"], "After the change");
    assert_eq!(e["calendar"]["title"], "Work");
    assert_eq!(e["etag"], "etag-E-DST");
    assert_eq!(e["start"]["in_event_zone"], "2026-03-29T09:30:00+02:00");
    assert!(e["start"].get("in_requested_zone").is_none());
    assert_eq!(got["complete_representation"], false);
    let missing = got["not_included"].as_array().unwrap();
    let words = serde_json::to_string(missing).unwrap();
    for kept_out in ["attendees", "alerts", "repeat rule", "status"] {
        assert!(words.contains(kept_out), "{kept_out}: {words}");
    }
    assert_eq!(
        error(&mut server, "calendar_get", json!({"guid":"DAV-1"})).await,
        "not_found"
    );
    assert_eq!(
        error(&mut server, "calendar_get", json!({"guid":"nope"})).await,
        "not_found"
    );
    assert_eq!(
        error(&mut server, "calendar_get", json!({"guid":""})).await,
        "invalid_arguments"
    );
}

#[tokio::test]
async fn search_finds_icloud_matches_behind_many_from_other_calendars() {
    let (dir, mut server) = seeded(None);
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // More matches than the search keeps, all in another account's calendar and nearer
    // today than the one iCloud match, five years out.
    let dav = format!("{}acct/home", quince_core::dav::PREFIX);
    let near: Vec<quince_core::calendar::Event> = (0..350)
        .map(|i| quince_core::calendar::Event {
            guid: format!("DAV-R{i}"),
            calendar_guid: dav.clone(),
            title: "Quarterly review".into(),
            starts_at: now + i * 60,
            ends_at: now + i * 60 + 60,
            timezone: Some("UTC".into()),
            ..Default::default()
        })
        .collect();
    library.put_events(&near).unwrap();
    library
        .put_events(&[quince_core::calendar::Event {
            guid: "ICLOUD-R".into(),
            calendar_guid: "cal-work".into(),
            title: "Quarterly review".into(),
            starts_at: now + 5 * 365 * 86_400,
            ends_at: now + 5 * 365 * 86_400 + 3600,
            timezone: Some("UTC".into()),
            ..Default::default()
        }])
        .unwrap();
    init(&mut server, false).await;
    let found = ok(
        &mut server,
        "calendar_search",
        json!({"query":"quarterly review"}),
    )
    .await;
    assert_eq!(guids(&found), ["ICLOUD-R"], "{found}");
    assert_eq!(found["may_be_incomplete"], false);
}

#[tokio::test]
async fn reads_report_cache_freshness_and_coverage_honestly() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    // Seeded without a refresh window: nothing is claimed as covered.
    let page = events(&mut server, "2026-10-05", "2026-10-06", "UTC").await;
    assert_eq!(page["coverage"]["range_within_last_refresh"], false);
    assert_eq!(page["complete_representation"], false);
    assert_eq!(page["remote_verified"], false);
    assert!(page["cache"]["refresh"]
        .as_str()
        .unwrap()
        .contains("calendar_refresh"));
}

#[tokio::test]
async fn writes_only_and_yolo_read_calendar_without_a_prompt() {
    for mode in ["writes_only", "yolo"] {
        let (_dir, mut server) = seeded(Some(mode));
        init(&mut server, true).await;
        for (name, args) in [
            ("calendar_calendars", json!({})),
            (
                "calendar_events",
                json!({"from":"2026-10-05","to":"2026-10-06","timezone":"UTC"}),
            ),
            ("calendar_search", json!({"query":"gym"})),
            ("calendar_get", json!({"guid":"F-1"})),
        ] {
            let reply = call(&mut server, 3, name, args).await;
            assert!(reply.get("method").is_none(), "{mode} {name}: {reply}");
            assert_eq!(reply["result"]["isError"], false, "{mode} {name}: {reply}");
        }
    }
}

#[tokio::test]
async fn reads_and_writes_binds_each_calendar_read_to_one_approval() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(&mut server, 3, "calendar_get", json!({"guid":"F-1"})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("calendar\\u005fget"), "{message}");
    assert!(!message.contains("Gym"), "no content before approval");
    assert_eq!(
        text(&call(&mut server, 4, "calendar_calendars", json!({})).await)["error"],
        "approval_pending"
    );
    let accepted = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(text(&accepted)["event"]["title"], "Gym");
    let form = call(&mut server, 5, "calendar_calendars", json!({})).await;
    let denied = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&denied)["error"], "approval_denied");
    // Malformed arguments are refused before anyone is asked.
    assert_eq!(
        text(
            &call(
                &mut server,
                6,
                "calendar_events",
                json!({"from":"2026-10-05","to":"2027-10-06","timezone":"UTC"})
            )
            .await
        )["error"],
        "range_too_wide"
    );
    // And without form support, nothing is read.
    let (_dir, mut plain) = seeded(Some("reads_and_writes"));
    init(&mut plain, false).await;
    assert_eq!(
        error(&mut plain, "calendar_search", json!({"query":"gym"})).await,
        "approval_requires_form_client"
    );
}

#[tokio::test]
async fn refresh_prompt_names_calendar_only_and_sends_nothing() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(
        &mut server,
        3,
        "calendar_refresh",
        json!({"from":"2026-10-01","to":"2026-11-01"}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("calendar\\u005frefresh"), "{message}");
    assert!(
        message.contains("\"services\": \\u005b\n      \"calendar\"\n    \\u005d"),
        "{message}"
    );
    assert!(message.contains("2026-10-01"), "{message}");
    assert!(
        !message.contains("notes")
            && !message.contains("reminders")
            && !message.contains("contacts"),
        "{message}"
    );
    assert!(message.contains("nothing is sent"), "{message}");
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_denied");
}

#[tokio::test]
async fn refresh_validates_before_any_keyring_or_network_use() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    assert_eq!(
        error(&mut server, "calendar_refresh", json!({})).await,
        "login_required"
    );
    for (args, code) in [
        (json!({"from":"2026-01-01"}), "invalid_arguments"),
        (
            json!({"from":"2026-01-01","to":"2026-07-07"}),
            "range_too_wide",
        ),
        (
            json!({"from":"2026-01-02","to":"2026-01-01"}),
            "invalid_range",
        ),
        (json!({"full":true}), "invalid_arguments"),
    ] {
        assert_eq!(
            error(&mut server, "calendar_refresh", args.clone()).await,
            code,
            "{args}"
        );
    }
}

// --- Refresh against a synthetic Apple.

async fn apple() -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"ckdatabasews":{"url":remote.uri()},"calendar":{"url":remote.uri()},"contacts":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    remote
}

async fn answer(remote: &MockServer, body: Value) {
    for p in ["/ca/startup", "/ca/events"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(remote)
            .await;
    }
}

fn saved_client(dir: &std::path::Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(
                json!({"ckdatabasews":{"url":uri},"calendar":{"url":uri},"contacts":{"url":uri}}),
            ),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

/// A bound directory with unrelated Notes, Reminders and Contacts state and queued writes
/// of every service, Calendar's included.
fn bound_with_unrelated_state(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            title: "Synthetic".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .mark_synced("reminders", Some("REMINDERS-CURSOR"))
        .unwrap();
    library.mark_synced("notes", Some("NOTES-CURSOR")).unwrap();
    library
        .mark_synced("contacts", Some("CONTACTS-CURSOR"))
        .unwrap();
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    for (service, record) in [
        ("notes", "N-1"),
        ("reminders", "R-1"),
        ("contacts", "C-1"),
        ("calendar", "E-QUEUED"),
    ] {
        library
            .outbox_append(service, record, "delete", "{\"queued\":true}")
            .unwrap();
    }
    (dir, server)
}

fn unrelated_snapshot(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let pending: Vec<Value> = library
        .outbox_all_pending()
        .unwrap()
        .into_iter()
        .map(|p| json!([p.id, p.service, p.record, p.kind, p.payload, p.tries]))
        .collect();
    let mut states = Vec::new();
    for service in ["notes", "reminders", "contacts"] {
        states.push(json!([
            library.sync_token(service).unwrap(),
            library.last_synced(service).unwrap(),
        ]));
    }
    json!({
        "pending": pending,
        "refused": library.outbox_refused_count().unwrap(),
        "states": states,
        "lists": library.reminder_lists().unwrap().len(),
        "policy": library.state("mcp.policy").unwrap(),
    })
}

/// Only Calendar reads: GETs of a window, each at most 28 days, asked in UTC.
fn only_calendar_reads(requests: &[wiremock::Request]) {
    for r in requests {
        let p = r.url.path();
        if p == "/setup/ws/1/validate" {
            continue;
        }
        assert_eq!(r.method.as_str(), "GET", "unexpected {} {p}", r.method);
        assert!(p == "/ca/startup" || p == "/ca/events", "unexpected {p}");
        let q: std::collections::HashMap<String, String> =
            r.url.query_pairs().into_owned().collect();
        assert_eq!(q["usertz"], "UTC");
        assert!(!q.contains_key("methodOverride"));
        let day = |s: &str| {
            let n: Vec<i64> = s.split('-').map(|x| x.parse().unwrap()).collect();
            n[0] * 372 + n[1] * 31 + n[2]
        };
        assert!(day(&q["endDate"]) >= day(&q["startDate"]));
        assert!(day(&q["endDate"]) - day(&q["startDate"]) < 31, "{q:?}");
    }
}

/// Files written by the synthetic session store are made private, as login leaves them.
fn private_files(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

async fn refresh(dir: &std::path::Path, uri: &str, args: Value) -> Result<Value, &'static str> {
    let result =
        skykeep_mcp::calendar::refresh_with_client(dir, saved_client(dir, uri), args).await;
    private_files(dir);
    result
}

#[tokio::test]
async fn refresh_reads_only_apple_calendar_and_never_sends_queued_writes() {
    for policy in [None, Some("writes_only"), Some("yolo")] {
        let remote = apple().await;
        answer(&remote, apple_body()).await;
        let (dir, _server) = bound_with_unrelated_state(policy);
        let before = unrelated_snapshot(dir.path());
        let result = refresh(
            dir.path(),
            &remote.uri(),
            json!({"from":"2026-03-01","to":"2026-04-01"}),
        )
        .await
        .unwrap();
        assert_eq!(result["service"], "calendar", "{policy:?}");
        assert_eq!(result["outcome"], "ok");
        assert_eq!(result["cached_calendars"], 2);
        assert_eq!(result["queued_calendar_changes"], 1);
        assert_eq!(result["queued_changes_sent"], 0);
        assert_eq!(result["remote_verified"], false);
        assert_eq!(
            result["window"],
            json!({"from":"2026-03-01","to":"2026-04-01","to_is_exclusive":true,"asked_apple_in":"UTC"})
        );
        assert!(!result.to_string().contains("SYNTHETIC-TOKEN"));
        let requests = remote.received_requests().await.unwrap();
        only_calendar_reads(&requests);
        // 2026-02-28 .. 2026-04-01, two windows, two reads each.
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path().starts_with("/ca/"))
                .count(),
            4
        );
        assert_eq!(unrelated_snapshot(dir.path()), before, "{policy:?}");
    }
}

#[tokio::test]
async fn refreshed_events_read_back_with_the_window_they_were_read_in() {
    let remote = apple().await;
    answer(&remote, apple_body()).await;
    let (dir, mut server) = bound_with_unrelated_state(Some("writes_only"));
    refresh(
        dir.path(),
        &remote.uri(),
        json!({"from":"2026-10-01","to":"2026-12-01"}),
    )
    .await
    .unwrap();
    init(&mut server, false).await;
    let inside = events(&mut server, "2026-10-05", "2026-10-06", "Asia/Kolkata").await;
    assert_eq!(guids(&inside), ["A-1", "F-1"]);
    assert_eq!(inside["coverage"]["range_within_last_refresh"], true);
    assert_eq!(
        inside["coverage"]["last_refresh_window"],
        json!({"from":"2026-10-01","to":"2026-12-01","to_is_exclusive":true})
    );
    assert_eq!(inside["cache"]["refreshed"], true);
    assert_eq!(inside["cache"]["last_refresh_failed"], false);
    // Apple is asked about one day either side of the window, and that day counts as
    // covered; past it the cache may hold events, but makes no claim about them.
    let margin = events(&mut server, "2026-12-01", "2026-12-02", "UTC").await;
    assert_eq!(margin["coverage"]["range_within_last_refresh"], true);
    let outside = events(&mut server, "2026-12-02", "2026-12-03", "UTC").await;
    assert_eq!(outside["coverage"]["range_within_last_refresh"], false);
    let straddling = events(&mut server, "2026-11-20", "2026-12-05", "UTC").await;
    assert_eq!(straddling["coverage"]["range_within_last_refresh"], false);
}

#[tokio::test]
async fn a_cancelled_occurrence_goes_and_events_outside_the_window_stay() {
    let remote = apple().await;
    answer(&remote, apple_body()).await;
    let (dir, mut server) = bound_with_unrelated_state(Some("yolo"));
    refresh(
        dir.path(),
        &remote.uri(),
        json!({"from":"2026-09-01","to":"2026-12-01"}),
    )
    .await
    .unwrap();
    // Seeded beyond the window, as an earlier read might have left it.
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .put_events(&[quince_core::calendar::Event {
                guid: "LATER".into(),
                calendar_guid: "cal-work".into(),
                title: "Next year".into(),
                starts_at: 1_800_000_000,
                ends_at: 1_800_003_600,
                timezone: Some("UTC".into()),
                ..Default::default()
            }])
            .unwrap();
    }
    // The organiser cancels one occurrence: Apple stops sending it.
    remote.reset().await;
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"calendar":{"url":uri}}
        })))
        .mount(&remote)
        .await;
    let mut body = apple_body();
    body["Event"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["guid"] != "WEEKLY__20261116T090000Z");
    answer(&remote, body).await;
    refresh(
        dir.path(),
        &remote.uri(),
        json!({"from":"2026-09-01","to":"2026-12-01"}),
    )
    .await
    .unwrap();
    init(&mut server, false).await;
    let nov = events(&mut server, "2026-11-01", "2026-12-01", "UTC").await;
    assert_eq!(
        guids(&nov),
        [
            "E-FOLD",
            "WEEKLY__20261102T090000Z",
            "WEEKLY__20261109T090000Z"
        ]
    );
    assert_eq!(
        error(
            &mut server,
            "calendar_get",
            json!({"guid":"WEEKLY__20261116T090000Z"})
        )
        .await,
        "not_found"
    );
    // Outside the window Apple was asked about, nothing is assumed gone.
    assert_eq!(
        ok(&mut server, "calendar_get", json!({"guid":"LATER"})).await["event"]["title"],
        "Next year"
    );
}

fn calendar_failed(dir: &std::path::Path) -> bool {
    quince_core::Library::open(dir)
        .unwrap()
        .sync_health()
        .unwrap()
        .iter()
        .any(|h| h.service == "calendar" && h.last_error.is_some())
}

fn cached(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    json!({
        "calendars": library.calendars().unwrap(),
        "events": library.events_between(i64::MIN / 4, i64::MAX / 4).unwrap(),
        "token": library.sync_token("calendar").unwrap(),
    })
}

#[tokio::test]
async fn a_failed_window_stores_nothing_and_keeps_apple_text_out() {
    let remote = apple().await;
    answer(&remote, apple_body()).await;
    let (dir, mut server) = bound_with_unrelated_state(Some("writes_only"));
    refresh(
        dir.path(),
        &remote.uri(),
        json!({"from":"2026-10-01","to":"2026-12-01"}),
    )
    .await
    .unwrap();
    let kept = cached(dir.path());
    let before = unrelated_snapshot(dir.path());
    // The first window's startup read answers with no events, then its events read is
    // refused as busy. Stored window by window, the empty answer would have deleted October.
    remote.reset().await;
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"calendar":{"url":uri}}
        })))
        .mount(&remote)
        .await;
    Mock::given(method("GET"))
        .and(path("/ca/events"))
        .respond_with(ResponseTemplate::new(503).set_body_string("SYNTHETIC-UPSTREAM-BODY"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&remote)
        .await;
    answer(&remote, json!({"Collection": collections(), "Event": []})).await;
    assert_eq!(
        refresh(
            dir.path(),
            &remote.uri(),
            json!({"from":"2026-10-01","to":"2026-12-01"})
        )
        .await
        .unwrap_err(),
        "calendar_refresh_failed_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert_eq!(unrelated_snapshot(dir.path()), before);
    assert!(calendar_failed(dir.path()));
    init(&mut server, false).await;
    let page = events(&mut server, "2026-10-05", "2026-10-06", "UTC").await;
    assert_eq!(guids(&page), ["A-1", "F-1"]);
    assert_eq!(page["cache"]["last_refresh_failed"], true);
    assert!(!page.to_string().contains("SYNTHETIC-UPSTREAM-BODY"));
    let health = quince_core::Library::open(dir.path())
        .unwrap()
        .sync_health()
        .unwrap();
    assert!(!format!("{health:?}").contains("SYNTHETIC-UPSTREAM-BODY"));
}

#[tokio::test]
async fn an_answer_without_calendars_is_refused_not_stored() {
    let remote = apple().await;
    answer(&remote, json!({"Collection": [], "Event": []})).await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(
            dir.path(),
            &remote.uri(),
            json!({"from":"2026-10-01","to":"2026-11-01"})
        )
        .await
        .unwrap_err(),
        "calendar_refresh_failed_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn refresh_fails_closed_on_session_account_or_service_problems() {
    // Apple ends the session for Calendar.
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/ca/startup"))
        .respond_with(ResponseTemplate::new(450))
        .mount(&remote)
        .await;
    let (dir, _server) = bound_with_unrelated_state(Some("writes_only"));
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "login_required"
    );
    // A session for another account reads nothing.
    let remote = apple().await;
    let dir = private_dir();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    store_body(&mut library, &apple_body());
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "account_mismatch_use_new_state_directory"
    );
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().starts_with("/ca/")));
    assert_eq!(cached(dir.path()), kept);
    // No Calendar service in the session.
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"ckdatabasews":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let dir = private_dir();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    let store = Store::file(dir.path()).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":remote.uri()}})),
            ..Default::default()
        })
        .unwrap();
    let client = Client::with(dir.path(), Endpoints::at(&remote.uri()), store).unwrap();
    assert_eq!(
        skykeep_mcp::calendar::refresh_with_client(dir.path(), client, json!({}))
            .await
            .unwrap_err(),
        "calendar_service_unavailable"
    );
    // Unbound directory: nothing is resumed or read.
    let remote = apple().await;
    let dir = private_dir();
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "login_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_and_writes_refresh_without_approval_touches_nothing() {
    let remote = apple().await;
    let (dir, _server) = bound_with_unrelated_state(Some("reads_and_writes"));
    let before = unrelated_snapshot(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "read_approval_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
    assert_eq!(unrelated_snapshot(dir.path()), before);
}

fn small_limits() -> skykeep_mcp::calendar::RefreshLimits {
    skykeep_mcp::calendar::RefreshLimits {
        max_response_bytes: 4096,
        max_total_bytes: 16_384,
        max_events: 8,
        ..Default::default()
    }
}

async fn refresh_limited(
    dir: &std::path::Path,
    uri: &str,
    limits: skykeep_mcp::calendar::RefreshLimits,
) -> Result<Value, &'static str> {
    skykeep_mcp::calendar::refresh_with_client_limited(
        dir,
        saved_client(dir, uri),
        json!({"from":"2026-10-01","to":"2026-11-01"}),
        limits,
    )
    .await
}

#[tokio::test]
async fn oversized_answers_and_too_many_events_are_refused_and_the_cache_kept() {
    // One body over the per-response cap, counted as it arrives.
    let remote = apple().await;
    let huge = json!({"Collection": collections(), "Event": [timed("BIG","cal-work",&"x".repeat(8000),Some("UTC"),date(2026,10,2,9,0),date(2026,10,2,10,0))]});
    answer(&remote, huge).await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), small_limits())
            .await
            .unwrap_err(),
        "calendar_response_too_large_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(calendar_failed(dir.path()));
    // Every body small, but together over the refresh's total.
    let remote = apple().await;
    answer(
        &remote,
        json!({"Collection": collections(), "Event": [], "pad": "y".repeat(3000)}),
    )
    .await;
    let limits = skykeep_mcp::calendar::RefreshLimits {
        max_total_bytes: 9000,
        ..small_limits()
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), limits)
            .await
            .unwrap_err(),
        "calendar_response_too_large_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    // More events than a refresh keeps.
    let remote = apple().await;
    let many: Vec<Value> = (0..9)
        .map(|i| {
            timed(
                &format!("M-{i}"),
                "cal-work",
                "m",
                Some("UTC"),
                date(2026, 10, 3, i, 0),
                date(2026, 10, 3, i, 30),
            )
        })
        .collect();
    answer(&remote, json!({"Collection": collections(), "Event": many})).await;
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), small_limits())
            .await
            .unwrap_err(),
        "calendar_too_many_events_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    // Not JSON.
    let remote = apple().await;
    for p in ["/ca/startup", "/ca/events"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>SYNTHETIC</html>"))
            .mount(&remote)
            .await;
    }
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), small_limits())
            .await
            .unwrap_err(),
        "calendar_refresh_failed_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn a_slow_answer_times_out_stores_nothing_and_is_recorded() {
    let remote = apple().await;
    for p in ["/ca/startup", "/ca/events"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(apple_body())
                    .set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&remote)
            .await;
    }
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    let limits = skykeep_mcp::calendar::RefreshLimits {
        timeout: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), limits)
            .await
            .unwrap_err(),
        "calendar_refresh_timeout_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(calendar_failed(dir.path()));
}

#[tokio::test]
async fn an_account_change_during_refresh_stores_nothing() {
    let remote = apple().await;
    for p in ["/ca/startup", "/ca/events"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(apple_body())
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&remote)
            .await;
    }
    let (dir, _server) = bound_with_unrelated_state(Some("yolo"));
    let path_buf = dir.path().to_owned();
    let uri = remote.uri();
    let running = tokio::spawn(async move {
        refresh(
            &path_buf,
            &uri,
            json!({"from":"2026-10-01","to":"2026-11-01"}),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    assert_eq!(
        running.await.unwrap().unwrap_err(),
        "account_changed_during_refresh_cache_kept"
    );
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert!(library.calendars().unwrap().is_empty());
    assert!(!calendar_failed(dir.path()));
}

#[tokio::test]
async fn pages_fit_the_cap_with_huge_fields_and_say_what_they_cut() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    let long = "\u{1F600}<".repeat(60_000);
    let events: Vec<Value> = (0..10)
        .map(|i| {
            let mut e = timed(
                &format!("H-{i:02}"),
                "cal-work",
                &long,
                Some("UTC"),
                date(2026, 10, 1, 9, i),
                date(2026, 10, 1, 10, i),
            );
            e["location"] = json!(long);
            e["description"] = json!(long);
            e
        })
        .collect();
    store_body(
        &mut library,
        &json!({"Collection": collections(), "Event": events}),
    );
    init(&mut server, false).await;
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let reply = call(
            &mut server,
            9,
            "calendar_events",
            json!({"from":"2026-10-01","to":"2026-10-02","timezone":"UTC","limit":100,"offset":offset}),
        )
        .await;
        let frame = serde_json::to_string(&reply).unwrap().len();
        assert!(frame <= 262_144 + 1024, "{frame}");
        let page = text(&reply);
        assert!(page["summaries_truncated"].as_u64().unwrap() > 0);
        seen.extend(guids(&page));
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(seen.len(), 10);
    let got = call(&mut server, 10, "calendar_get", json!({"guid":"H-00"})).await;
    assert!(serde_json::to_string(&got).unwrap().len() <= 524_288 + 1024);
    let got = text(&got);
    assert_eq!(got["event_truncated"], true);
    assert!(got["truncation"]["max_chars_per_text"].as_u64().is_some());
}
