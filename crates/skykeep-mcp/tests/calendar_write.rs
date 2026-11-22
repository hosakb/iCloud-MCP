//! Apple iCloud Calendar changes through `calendar_write`: each action against a loopback fake
//! of Apple's calendar service, the approval each needs, versions, repeating events,
//! attendees and time zones. Every fixture is synthetic; nothing reaches the network.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::calendar_write::{Seam, WriteLimits};
use skykeep_mcp::Server;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DSID: &str = "1234567890";
const OCCURRENCE: &str = "WEEKLY__20261109T090000Z";

fn private_dir() -> tempfile::TempDir {
    unsafe {
        libc::umask(0o077);
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn date(y: i64, m: i64, d: i64, h: i64, min: i64) -> Value {
    json!([y * 10_000 + m * 100 + d, y, m, d, h, min, h * 60 + min])
}

fn timed(guid: &str, cal: &str, title: &str, tz: &str, start: Value, end: Value) -> Value {
    json!({"guid":guid,"pGuid":cal,"title":title,"tz":tz,"startDate":start,"endDate":end,"etag":format!("etag-{guid}")})
}

fn collections() -> Value {
    json!([
        {"guid":"cal-work","title":"Work","isDefault":true,"ctag":"c1"},
        {"guid":"cal-home","title":"Home","ctag":"c2"},
        {"guid":"cal-shared","title":"Team (shared)","readOnly":true,"ctag":"c3"}
    ])
}

fn apple_events() -> Vec<Value> {
    let mut weekly = timed(
        OCCURRENCE,
        "cal-work",
        "Standup",
        "UTC",
        date(2026, 11, 9, 9, 0),
        date(2026, 11, 9, 9, 15),
    );
    weekly["masterStartDate"] = date(2026, 11, 2, 9, 0);
    weekly["masterEndDate"] = date(2026, 11, 2, 9, 15);
    vec![
        timed(
            "E-1",
            "cal-work",
            "Dentist",
            "Europe/Berlin",
            date(2026, 11, 10, 9, 0),
            date(2026, 11, 10, 10, 0),
        ),
        timed(
            "MEET",
            "cal-work",
            "Planning",
            "Europe/Berlin",
            date(2026, 11, 11, 14, 0),
            date(2026, 11, 11, 15, 0),
        ),
        timed(
            "MASTER",
            "cal-work",
            "Yoga",
            "Europe/Berlin",
            date(2026, 11, 12, 18, 0),
            date(2026, 11, 12, 19, 0),
        ),
        timed(
            "TEAM-1",
            "cal-shared",
            "Team day",
            "UTC",
            date(2026, 11, 13, 9, 0),
            date(2026, 11, 13, 17, 0),
        ),
        // Floating: no zone, 07:00 wherever it is looked at.
        json!({"guid":"FLOAT","pGuid":"cal-work","title":"Run","etag":"etag-FLOAT",
            "startDate":date(2026,11,14,7,0),"endDate":date(2026,11,14,8,0)}),
        json!({"guid":"A-1","pGuid":"cal-work","title":"Holiday","allDay":true,"etag":"etag-A-1",
            "startDate":date(2026,10,5,0,0),"endDate":date(2026,10,6,0,0)}),
        weekly,
    ]
}

/// Apple's calendar service as far as these tests need it: events kept by id, `eventdetail`
/// answering from them, and saves and deletes that check the version sent.
#[derive(Clone, Default)]
struct Fake {
    events: Arc<Mutex<BTreeMap<String, Value>>>,
    /// Per event: the `Invitee` and `Recurrence` objects `eventdetail` returns beside it.
    extra: Arc<Mutex<BTreeMap<String, Value>>>,
    version: Arc<Mutex<u64>>,
    /// Saves and deletes wait this long before answering.
    delay: Arc<Mutex<Option<Duration>>>,
    /// `eventdetail` for an occurrence answers with its series alone, as Apple may.
    master_only: Arc<Mutex<bool>>,
}

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let p = request.url.path().to_string();
        if let Some(rest) = p.strip_prefix("/ca/eventdetail/") {
            let guid = rest.split_once('/').map(|(_, g)| g).unwrap_or_default();
            let events = self.events.lock().unwrap();
            let Some(event) = events.get(guid) else {
                return ResponseTemplate::new(404);
            };
            let mut event = event.clone();
            if *self.master_only.lock().unwrap() {
                if let Some((series, _)) = guid.split_once("__") {
                    event["guid"] = json!(series);
                }
            }
            let mut body = json!({"Event":[event],"Alarm":[]});
            if let Some(extra) = self.extra.lock().unwrap().get(guid) {
                for (k, v) in extra.as_object().unwrap() {
                    body[k] = v.clone();
                }
            }
            return ResponseTemplate::new(200).set_body_json(body);
        }
        let Some(rest) = p.strip_prefix("/ca/events/") else {
            return ResponseTemplate::new(400);
        };
        let mut parts = rest.split('/');
        let (_cal, guid) = (parts.next().unwrap(), parts.next().unwrap().to_string());
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let deleting = request
            .url
            .query_pairs()
            .any(|(k, v)| k == "methodOverride" && v == "DELETE");
        let mut events = self.events.lock().unwrap();
        let held = events.get(&guid).cloned();
        if let Some(held) = &held {
            if held["etag"] != body["Event"]["etag"] {
                return ResponseTemplate::new(412);
            }
        }
        let template = |t: ResponseTemplate| match *self.delay.lock().unwrap() {
            Some(d) => t.set_delay(d),
            None => t,
        };
        if deleting {
            if events.remove(&guid).is_none() {
                return ResponseTemplate::new(404);
            }
            return template(ResponseTemplate::new(200).set_body_json(json!({})));
        }
        let mut stored = body["Event"].clone();
        let mut version = self.version.lock().unwrap();
        *version += 1;
        stored["etag"] = json!(format!("v{version}"));
        if let Some(old) = stored.as_object_mut().unwrap().remove("oldpGuid") {
            assert!(old.is_string());
        }
        events.insert(guid, stored.clone());
        template(ResponseTemplate::new(200).set_body_json(json!({"Event":[stored]})))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    server: Server,
    remote: MockServer,
    fake: Fake,
}

fn client(dir: &Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"calendar":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

fn seam(uri: &str, limits: WriteLimits) -> Seam {
    let uri = uri.to_string();
    Seam {
        client: Some(Arc::new(move |dir: &Path| Ok(client(dir, &uri)))),
        limits,
    }
}

async fn fixture(policy: Option<&str>) -> Fixture {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"calendar":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let fake = Fake::default();
    for e in apple_events() {
        fake.events
            .lock()
            .unwrap()
            .insert(e["guid"].as_str().unwrap().to_string(), e);
    }
    fake.extra.lock().unwrap().insert(
        "MEET".into(),
        json!({"Invitee":[
            {"guid":"MEET:me","pGuid":"MEET","email":"mailto:owner@example.test","isMe":true,"isOrganizer":true,"inviteeStatus":"ACCEPTED"},
            {"guid":"MEET:guest","pGuid":"MEET","email":"mailto:guest@example.test","inviteeStatus":"NEEDS-ACTION"}
        ]}),
    );
    fake.extra.lock().unwrap().insert(
        "MASTER".into(),
        json!({"Recurrence":[{"guid":"MASTER*MME-RID","pGuid":"MASTER","freq":"weekly","interval":1}]}),
    );
    Mock::given(path_regex("^/ca/(eventdetail|events)/"))
        .respond_with(fake.clone())
        .mount(&remote)
        .await;
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
        let snapshot = quince_icloud::calendar::parse_snapshot(
            &json!({"Collection":collections(),"Event":apple_events()}),
        );
        quince_icloud::sync::apply_calendar(&mut library, snapshot).unwrap();
    }
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    server.set_calendar_seam(seam(
        &remote.uri(),
        WriteLimits {
            change_timeout: Duration::from_secs(20),
        },
    ));
    init(&mut server, true).await;
    Fixture {
        dir,
        server,
        remote,
        fake,
    }
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

async fn call(server: &mut Server, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"calendar_write","arguments":args}}))
        .await
        .unwrap()
}

async fn ok(server: &mut Server, args: Value) -> Value {
    let reply = call(server, args).await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    text(&reply)
}

async fn error(server: &mut Server, args: Value) -> Value {
    let reply = call(server, args).await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    text(&reply)
}

async fn code(server: &mut Server, args: Value) -> String {
    error(server, args).await["error"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The saves and deletes sent to Apple: path, and the body.
async fn writes(remote: &MockServer) -> Vec<(String, Value)> {
    remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/ca/events/"))
        .map(|r| {
            let mut p = r.url.path().to_string();
            if r.url
                .query_pairs()
                .any(|(k, v)| k == "methodOverride" && v == "DELETE")
            {
                p.insert_str(0, "DELETE ");
            }
            (p, serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        })
        .collect()
}

async fn write_paths(remote: &MockServer) -> Vec<String> {
    writes(remote).await.into_iter().map(|(p, _)| p).collect()
}

fn cached(dir: &Path, guid: &str) -> Option<quince_core::calendar::Event> {
    quince_core::Library::open(dir)
        .unwrap()
        .event(guid)
        .unwrap()
}

fn rules(dir: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(skykeep_mcp::permissions::FILE_NAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn choices(form: &Value) -> Vec<String> {
    form["params"]["requestedSchema"]["properties"]["decision"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["const"].as_str().unwrap().to_string())
        .collect()
}

fn answer(form: &Value, decision: &str) -> Value {
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":decision}}})
}

fn proposal_id(value: &Value) -> String {
    assert_eq!(value["status"], "approval_required", "{value}");
    value["request_id"].as_str().unwrap().to_string()
}

fn update(guid: &str, extra: Value) -> Value {
    let mut args = json!({"action":"update","guid":guid,"etag":format!("etag-{guid}")});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

#[tokio::test]
async fn the_tool_is_listed_strict_and_its_actions_are_rule_names() {
    let mut f = fixture(Some("yolo")).await;
    let list = f
        .server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .await
        .unwrap();
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "calendar_write")
        .unwrap()
        .clone();
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        tool["inputSchema"]["properties"]["action"]["enum"],
        json!(["create", "update", "move", "delete"])
    );
    let names = skykeep_mcp::permissions::names();
    for name in [
        "calendar_write",
        "calendar_write.create",
        "calendar_write.update",
        "calendar_write.move",
        "calendar_write.delete",
        "calendar_write.send_invitations",
    ] {
        assert!(names.iter().any(|n| n == name), "{name}");
    }
    // Deletion is decided by the one shared helper.
    for action in skykeep_mcp::calendar_write::ACTIONS {
        assert_eq!(
            skykeep_mcp::calendar_write::is_destructive(action),
            skykeep_mcp::permissions::is_deletion("calendar_write", action)
        );
    }
    assert_eq!(
        code(
            &mut f.server,
            update("E-1", json!({"title":"x","colour":"red"}))
        )
        .await,
        "invalid_arguments"
    );
    assert!(write_paths(&f.remote).await.is_empty());
}

#[tokio::test]
async fn date_errors_say_the_accepted_format() {
    let mut f = fixture(Some("yolo")).await;
    for start in [
        "2026-11-20 09:00",
        "20.11.2026",
        "2026-11-20T09:00:30Z",
        "soon",
    ] {
        let e = error(
            &mut f.server,
            json!({"action":"create","calendar_guid":"cal-work","title":"T","start":start,"timezone":"Europe/Berlin"}),
        )
        .await;
        assert_eq!(e["error"], "invalid_start", "{start}");
        let said = e["instructions"].as_str().unwrap();
        assert!(said.contains("2026-03-29T09:30:00+02:00"), "{said}");
        assert!(
            said.contains("Europe/Berlin") && said.contains("all-day"),
            "{said}"
        );
    }
    // A local time with no zone is refused, never guessed.
    let e = error(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"T","start":"2026-11-20T09:00"}),
    )
    .await;
    assert_eq!(e["error"], "timezone_required");
    assert!(e["instructions"].as_str().unwrap().contains("+02:00"));
    assert_eq!(
        code(
            &mut f.server,
            json!({"action":"create","calendar_guid":"cal-work","title":"T","start":"2026-11-20T09:00","timezone":"Mars/Olympus"})
        )
        .await,
        "unknown_timezone"
    );
    assert_eq!(
        code(
            &mut f.server,
            json!({"action":"create","calendar_guid":"cal-work","title":"T","start":"2026-11-20T10:00Z","end":"2026-11-20T09:00Z","timezone":"UTC"})
        )
        .await,
        "end_must_be_after_start"
    );
    assert!(write_paths(&f.remote).await.is_empty());
}

#[tokio::test]
async fn daylight_saving_edges_are_refused_or_placed_exactly() {
    let mut f = fixture(Some("yolo")).await;
    let create = |start: &str, end: &str| json!({"action":"create","calendar_guid":"cal-work","title":"Edge","start":start,"end":end,"timezone":"Europe/Berlin"});
    // Berlin skips 02:00-03:00 on 2026-03-29 and passes 02:00-03:00 twice on 2026-10-25.
    let e = error(
        &mut f.server,
        create("2026-03-29T02:30", "2026-03-29T04:00"),
    )
    .await;
    assert_eq!(e["error"], "time_does_not_exist_in_timezone_clocks_skip_it");
    let e = error(
        &mut f.server,
        create("2026-10-25T02:30", "2026-10-25T04:00"),
    )
    .await;
    assert_eq!(e["error"], "time_is_ambiguous_in_timezone_give_an_offset");
    assert!(e["instructions"].as_str().unwrap().contains("offset"));
    assert!(write_paths(&f.remote).await.is_empty());
    // Apple keeps the clock reading, which there means the first pass: the second 02:30
    // (winter time, +01:00) can't be kept and is refused rather than saved an hour early.
    let e = error(
        &mut f.server,
        create("2026-10-25T02:30:00+01:00", "2026-10-25T04:00"),
    )
    .await;
    assert_eq!(
        e["error"],
        "time_in_the_repeated_hour_apple_keeps_only_the_first"
    );
    assert!(write_paths(&f.remote).await.is_empty());
    // The offset of the first pass (summer time) is placed exactly and read back the same.
    let done = ok(
        &mut f.server,
        create("2026-10-25T02:30:00+02:00", "2026-10-25T04:00"),
    )
    .await;
    assert_eq!(done["remote_verified"], true, "{done}");
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(body["Event"]["tz"], "Europe/Berlin");
    assert_eq!(body["Event"]["startDate"], date(2026, 10, 25, 2, 30));
    // 00:30 UTC to 03:00 UTC: the night the clocks went back, this is 150 minutes.
    assert_eq!(body["Event"]["duration"], 150);
    // Across the spring change: 01:00 to 04:00 local is two hours.
    ok(
        &mut f.server,
        create("2026-03-29T01:00", "2026-03-29T04:00"),
    )
    .await;
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(body["Event"]["duration"], 120);
    assert_eq!(body["Event"]["endDate"], date(2026, 3, 29, 4, 0));
}

#[tokio::test]
async fn create_saves_reads_back_and_caches_the_event_with_its_alerts() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"Flight","start":"2026-12-01T07:15:00Z","end":"2026-12-01T09:00:00Z","timezone":"Europe/Lisbon","location":"LIS","alerts":[30,0]}),
    )
    .await;
    assert_eq!(done["outcome"], "done");
    assert_eq!(done["remote_verified"], true, "{done}");
    let guid = done["event"]["guid"].as_str().unwrap().to_string();
    let sent = writes(&f.remote).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, format!("/ca/events/cal-work/{guid}"));
    let body = &sent[0].1;
    assert_eq!(body["Event"]["title"], "Flight");
    assert_eq!(body["Alarm"].as_array().unwrap().len(), 2);
    assert_eq!(body["Event"]["invitees"], json!([]));
    let event = cached(f.dir.path(), &guid).unwrap();
    assert_eq!(event.title, "Flight");
    assert_eq!(event.etag.as_deref(), Some("v1"));
    // Read-only and unknown calendars are refused before anything is sent.
    for (cal, why) in [
        ("cal-shared", "calendar_read_only"),
        ("cal-none", "calendar_not_found"),
    ] {
        assert_eq!(
            code(
                &mut f.server,
                json!({"action":"create","calendar_guid":cal,"title":"x","start":"2026-12-01T07:15Z","timezone":"UTC"})
            )
            .await,
            why
        );
    }
    assert_eq!(writes(&f.remote).await.len(), 1);
}

#[tokio::test]
async fn all_day_events_take_dates_with_an_exclusive_end() {
    let mut f = fixture(Some("yolo")).await;
    ok(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-home","title":"Trip","all_day":true,"start":"2026-10-24","end":"2026-10-27"}),
    )
    .await;
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(body["Event"]["allDay"], true);
    assert_eq!(body["Event"]["tz"], Value::Null);
    // The dates as Apple writes them, whatever this computer's zone (a DST change inside).
    assert_eq!(
        body["Event"]["startDate"].as_array().unwrap()[..4],
        date(2026, 10, 24, 0, 0).as_array().unwrap()[..4]
    );
    assert_eq!(
        body["Event"]["endDate"].as_array().unwrap()[..4],
        date(2026, 10, 27, 0, 0).as_array().unwrap()[..4]
    );
    // A time for an all-day event, or an end not after the start, is refused.
    for (start, end, why) in [
        ("2026-10-24T09:00Z", "2026-10-25", "invalid_start"),
        ("2026-10-24", "2026-10-24", "end_must_be_after_start"),
    ] {
        assert_eq!(
            code(
                &mut f.server,
                json!({"action":"create","calendar_guid":"cal-home","title":"x","all_day":true,"start":start,"end":end})
            )
            .await,
            why
        );
    }
    // An all-day event moved by a day keeps its length.
    let done = ok(&mut f.server, update("A-1", json!({"start":"2026-10-06"}))).await;
    assert_eq!(done["remote_verified"], true, "{done}");
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(
        body["Event"]["startDate"].as_array().unwrap()[..4],
        date(2026, 10, 6, 0, 0).as_array().unwrap()[..4]
    );
    assert_eq!(
        body["Event"]["endDate"].as_array().unwrap()[..4],
        date(2026, 10, 7, 0, 0).as_array().unwrap()[..4]
    );
}

#[tokio::test]
async fn update_keeps_what_is_not_given_and_refuses_a_changed_version() {
    let mut f = fixture(Some("yolo")).await;
    // An etag other than the cached one is refused before anything is asked of Apple.
    let mut stale = update("E-1", json!({"title":"Dentist (moved)"}));
    stale["etag"] = json!("etag-old");
    let e = error(&mut f.server, stale).await;
    assert_eq!(e["error"], "etag_not_current_run_calendar_get");
    assert!(e["instructions"].as_str().unwrap().contains("calendar_get"));
    // Apple's copy changed on another device since the cache read it: refused, not written over.
    f.fake.events.lock().unwrap().get_mut("E-1").unwrap()["etag"] = json!("etag-elsewhere");
    assert_eq!(
        code(&mut f.server, update("E-1", json!({"title":"Mine"}))).await,
        "calendar_event_changed_run_calendar_refresh"
    );
    assert!(write_paths(&f.remote).await.is_empty());
    f.fake.events.lock().unwrap().get_mut("E-1").unwrap()["etag"] = json!("etag-E-1");
    // The change: a new end and title; the start and zone stay.
    let done = ok(
        &mut f.server,
        update(
            "E-1",
            json!({"title":"Dentist, long","end":"2026-11-10T11:30"}),
        ),
    )
    .await;
    assert_eq!(done["remote_verified"], true, "{done}");
    let (p, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(p, "/ca/events/cal-work/E-1");
    assert_eq!(body["Event"]["etag"], "etag-E-1");
    assert_eq!(body["Event"]["startDate"], date(2026, 11, 10, 9, 0));
    assert_eq!(body["Event"]["endDate"], date(2026, 11, 10, 11, 30));
    assert_eq!(body["Event"]["tz"], "Europe/Berlin");
    // The cache has Apple's new version, so the old etag is no longer current.
    assert_eq!(
        cached(f.dir.path(), "E-1").unwrap().etag.as_deref(),
        Some("v1")
    );
    assert_eq!(
        code(&mut f.server, update("E-1", json!({"title":"again"}))).await,
        "etag_not_current_run_calendar_get"
    );
}

#[tokio::test]
async fn apple_refusing_the_version_at_save_time_is_a_change_elsewhere() {
    let mut f = fixture(Some("yolo")).await;
    // Apple's version check at the save itself (HTTP 412) is reported the same way.
    Mock::given(method("POST"))
        .and(path("/ca/events/cal-work/E-1"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(1)
        .mount(&f.remote)
        .await;
    assert_eq!(
        code(&mut f.server, update("E-1", json!({"title":"Mine"}))).await,
        "calendar_event_changed_run_calendar_refresh"
    );
}

#[tokio::test]
async fn a_repeating_event_needs_its_scope_said() {
    let mut f = fixture(Some("yolo")).await;
    let e = error(
        &mut f.server,
        update(OCCURRENCE, json!({"title":"Standup!"})),
    )
    .await;
    assert_eq!(e["error"], "scope_required_for_repeating_event");
    assert!(e["instructions"].as_str().unwrap().contains("whole_series"));
    assert_eq!(
        code(
            &mut f.server,
            json!({"action":"delete","guid":OCCURRENCE,"etag":format!("etag-{OCCURRENCE}")})
        )
        .await,
        "scope_required_for_repeating_event"
    );
    // A series cached under its own id is checked at Apple, and refused without a scope.
    assert_eq!(
        code(&mut f.server, update("MASTER", json!({"title":"Yoga!"}))).await,
        "scope_required_for_repeating_event"
    );
    assert_eq!(
        code(
            &mut f.server,
            update("MASTER", json!({"title":"Yoga!","scope":"this_occurrence"}))
        )
        .await,
        "this_occurrence_needs_an_occurrence_guid"
    );
    assert!(write_paths(&f.remote).await.is_empty());
    // This occurrence alone, and then the whole series.
    let done = ok(
        &mut f.server,
        update(
            OCCURRENCE,
            json!({"title":"Standup (once)","scope":"this_occurrence"}),
        ),
    )
    .await;
    assert_eq!(done["scope"], "this_occurrence");
    assert_eq!(done["remote_verified"], true, "{done}");
    let (p, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(p, format!("/ca/events/cal-work/{OCCURRENCE}/this"));
    assert_eq!(body["Event"]["recurrenceException"], true);
    let done = ok(
        &mut f.server,
        update(
            "MASTER",
            json!({"title":"Yoga (all)","scope":"whole_series"}),
        ),
    )
    .await;
    assert_eq!(done["remote_verified"], false, "{done}");
    assert!(done["checked"]
        .as_str()
        .unwrap()
        .contains("calendar_refresh"));
    assert_eq!(
        write_paths(&f.remote).await.pop().unwrap(),
        "/ca/events/cal-work/MASTER"
    );
}

#[tokio::test]
async fn events_with_other_attendees_are_refused_unless_the_rule_allows_it() {
    let mut f = fixture(Some("yolo")).await;
    let change = update("MEET", json!({"title":"Planning (new room)"}));
    let e = error(&mut f.server, change.clone()).await;
    assert_eq!(e["error"], "calendar_event_has_other_attendees_refused");
    assert!(e["instructions"]
        .as_str()
        .unwrap()
        .contains("calendar_write.send_invitations: allow"));
    // Only allow lets it run: a tool-wide allow is not enough, and ask is not a choice.
    rules(f.dir.path(), "rules:\n  calendar_write: allow\n");
    assert_eq!(
        code(&mut f.server, change.clone()).await,
        "calendar_event_has_other_attendees_refused"
    );
    assert!(skykeep_mcp::permissions::parse(
        "rules:\n  calendar_write.send_invitations: {ask: client}\n"
    )
    .is_err());
    // A broken file with no earlier valid rules refuses it too.
    rules(
        f.dir.path(),
        "rules:\n  calendar_write.send_invitations: maybe\n",
    );
    let mut fresh = fixture(Some("yolo")).await;
    rules(
        fresh.dir.path(),
        "rules:\n  calendar_write.send_invitations: maybe\n",
    );
    assert_eq!(
        code(&mut fresh.server, change.clone()).await,
        "calendar_event_has_other_attendees_refused"
    );
    assert!(write_paths(&f.remote).await.is_empty());
    rules(
        f.dir.path(),
        "rules:\n  calendar_write.send_invitations: allow\n",
    );
    let done = ok(&mut f.server, change).await;
    assert_eq!(done["attendees_notified"], 1, "{done}");
    assert_eq!(write_paths(&f.remote).await, ["/ca/events/cal-work/MEET"]);
}

#[tokio::test]
async fn the_preview_says_who_would_be_told_and_what_changes() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        update("MEET", json!({"title":"Planning v2"})),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    // The card's text escapes `_` and `.` stays.
    assert!(
        message.contains("send\\u005finvitations: allow"),
        "{message}"
    );
    assert!(message.contains("Planning v2"), "{message}");
    // A change that is not a delete may be always allowed.
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    let reply = f.server.handle(answer(&form, "deny")).await.unwrap();
    assert_eq!(text(&reply)["error"], "approval_denied");
    assert!(write_paths(&f.remote).await.is_empty());
}

#[tokio::test]
async fn deletes_always_ask_and_never_offer_always_allow() {
    let delete = json!({"action":"delete","guid":"E-1","etag":"etag-E-1"});
    // yolo and no mode: the terminal, even with an allow rule.
    for mode in [None, Some("yolo")] {
        let mut f = fixture(mode).await;
        rules(f.dir.path(), "rules:\n  calendar_write.delete: allow\n");
        let id = proposal_id(&ok(&mut f.server, delete.clone()).await);
        assert!(write_paths(&f.remote).await.is_empty(), "{mode:?}");
        let shown = f.server.approval_text(&id).unwrap();
        assert!(
            shown.contains("Calendar change: delete, a deletion"),
            "{shown}"
        );
        assert!(shown.contains("Dentist"), "{shown}");
        assert!(shown.contains("cannot undo"), "{shown}");
        let done = f.server.approve_any(&id).await.unwrap();
        assert_eq!(done["remote_verified"], true, "{done}");
        assert_eq!(
            write_paths(&f.remote).await,
            ["DELETE /ca/events/cal-work/E-1"]
        );
        assert!(cached(f.dir.path(), "E-1").is_none());
        // Used up: it runs once.
        assert!(f.server.approve_any(&id).await.is_err());
    }
    // The other modes: the card, with approve and deny only, even with an allow rule.
    for mode in ["writes_only", "reads_and_writes"] {
        let mut f = fixture(Some(mode)).await;
        rules(f.dir.path(), "rules:\n  calendar_write: allow\n");
        let mut form = call(&mut f.server, delete.clone()).await;
        assert_eq!(form["method"], "elicitation/create", "{mode}: {form}");
        if mode == "reads_and_writes" {
            form = f.server.handle(answer(&form, "approve")).await.unwrap();
            assert_eq!(form["method"], "elicitation/create", "{form}");
        }
        assert_eq!(choices(&form), ["approve", "deny"], "{mode}");
        assert!(write_paths(&f.remote).await.is_empty());
        let done = text(&f.server.handle(answer(&form, "approve")).await.unwrap());
        assert_eq!(done["outcome"], "done", "{done}");
        assert_eq!(
            write_paths(&f.remote).await,
            ["DELETE /ca/events/cal-work/E-1"]
        );
    }
}

#[tokio::test]
async fn a_terminal_approval_is_stale_after_the_event_changes_in_the_cache() {
    let mut f = fixture(Some("yolo")).await;
    let id = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"delete","guid":"E-1","etag":"etag-E-1"}),
        )
        .await,
    );
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut e = library.event("E-1").unwrap().unwrap();
        e.title = "Dentist (renamed elsewhere)".into();
        library.put_events(&[e]).unwrap();
    }
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("approval_stale_calendar_changed_ask_again")
    );
    // A deny rule set later refuses a waiting request too.
    let id = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"delete","guid":"E-1","etag":"etag-E-1"}),
        )
        .await,
    );
    rules(f.dir.path(), "rules:\n  calendar_write.delete: deny\n");
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("denied_by_permissions")
    );
    assert!(write_paths(&f.remote).await.is_empty());
}

#[tokio::test]
async fn deleting_one_occurrence_or_the_whole_series() {
    let mut f = fixture(Some("yolo")).await;
    let args = json!({"action":"delete","guid":OCCURRENCE,"etag":format!("etag-{OCCURRENCE}"),"scope":"this_occurrence"});
    let id = proposal_id(&ok(&mut f.server, args).await);
    assert!(f
        .server
        .approval_text(&id)
        .unwrap()
        .contains("this one occurrence"));
    let done = f.server.approve_any(&id).await.unwrap();
    assert_eq!(done["remote_verified"], false, "{done}");
    assert_eq!(
        write_paths(&f.remote).await,
        [format!("DELETE /ca/events/cal-work/{OCCURRENCE}/this")]
    );
    let id = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"delete","guid":"MASTER","etag":"etag-MASTER","scope":"whole_series"}),
        )
        .await,
    );
    assert!(f
        .server
        .approval_text(&id)
        .unwrap()
        .contains("every occurrence"));
    f.server.approve_any(&id).await.unwrap();
    assert!(cached(f.dir.path(), "MASTER").is_none());
}

#[tokio::test]
async fn move_puts_the_event_in_another_writable_calendar() {
    let mut f = fixture(Some("yolo")).await;
    let moving =
        |to: &str| json!({"action":"move","guid":"E-1","etag":"etag-E-1","calendar_guid":to});
    assert_eq!(
        code(&mut f.server, moving("cal-shared")).await,
        "calendar_read_only"
    );
    assert_eq!(
        code(&mut f.server, moving("cal-work")).await,
        "already_in_this_calendar"
    );
    // An event in a read-only calendar is not changed either.
    assert_eq!(
        code(
            &mut f.server,
            json!({"action":"move","guid":"TEAM-1","etag":"etag-TEAM-1","calendar_guid":"cal-home"})
        )
        .await,
        "calendar_read_only"
    );
    assert!(write_paths(&f.remote).await.is_empty());
    let done = ok(&mut f.server, moving("cal-home")).await;
    assert_eq!(done["remote_verified"], true, "{done}");
    let (p, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(p, "/ca/events/cal-home/E-1");
    assert_eq!(body["Event"]["oldpGuid"], "cal-work");
    assert_eq!(
        cached(f.dir.path(), "E-1").unwrap().calendar_guid,
        "cal-home"
    );
}

#[tokio::test]
async fn an_answer_lost_after_sending_is_unknown_and_says_to_look_first() {
    let mut f = fixture(Some("yolo")).await;
    *f.fake.delay.lock().unwrap() = Some(Duration::from_secs(5));
    f.server.set_calendar_seam(seam(
        &f.remote.uri(),
        WriteLimits {
            change_timeout: Duration::from_secs(1),
        },
    ));
    let e = error(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"Lost","start":"2026-12-02T10:00Z","timezone":"UTC"}),
    )
    .await;
    assert_eq!(
        e["error"],
        "calendar_write_outcome_unknown_run_calendar_refresh_before_retrying"
    );
    let said = e["instructions"].as_str().unwrap();
    assert!(
        said.contains("calendar_refresh") && said.contains("second event"),
        "{said}"
    );
}

#[tokio::test]
async fn writes_need_a_signed_in_folder_and_follow_the_mode() {
    // writes_only asks for a create on the card; without form support it is refused.
    let mut f = fixture(Some("writes_only")).await;
    let create = json!({"action":"create","calendar_guid":"cal-work","title":"New","start":"2026-12-03T10:00Z","timezone":"UTC"});
    let form = call(&mut f.server, create.clone()).await;
    assert_eq!(form["method"], "elicitation/create");
    let done = text(
        &f.server
            .handle(answer(&form, "always_allow"))
            .await
            .unwrap(),
    );
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(
        done["permission_rule"]["rule"],
        "calendar_write.create: allow"
    );
    // Saved: the next create runs without a card.
    let again = ok(&mut f.server, create.clone()).await;
    assert_eq!(again["outcome"], "done");
    let mut plain = Server::open(f.dir.path()).unwrap();
    init(&mut plain, false).await;
    let reply = call(
        &mut plain,
        json!({"action":"update","guid":"E-1","etag":"etag-E-1","title":"x"}),
    )
    .await;
    assert_eq!(text(&reply)["error"], "approval_requires_form_client");
    // A folder with no account bound.
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, true).await;
    server.set_policy("yolo").unwrap();
    assert_eq!(code(&mut server, create).await, "login_required");
}

#[tokio::test]
async fn permissions_show_says_how_the_calendar_rules_run() {
    let f = fixture(Some("writes_only")).await;
    rules(
        f.dir.path(),
        "rules:\n  calendar_write.delete: allow\n  calendar_write.send_invitations: allow\n",
    );
    let shown =
        skykeep_mcp::permissions::show(&f.dir.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        shown.contains("calendar_write.delete: allow, but asks in your MCP client, because a Calendar delete always needs a person"),
        "{shown}"
    );
    assert!(
        shown.contains("calendar_write.send_invitations: allow, a Calendar change to an event with other people invited may run"),
        "{shown}"
    );
    let starter = skykeep_mcp::permissions::starter(Some("writes_only"), Path::new("/srv/sky"));
    let parsed = skykeep_mcp::permissions::parse(&starter).unwrap();
    assert!(parsed.contains_key("calendar_write.delete"));
    assert!(!parsed.contains_key("calendar_write.send_invitations"));
}

// --- Review fixes.

#[tokio::test]
async fn offset_times_without_a_timezone_are_kept_in_utc() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"Call","start":"2026-12-04T09:00:00+01:00","end":"2026-12-04T10:00:00Z"}),
    )
    .await;
    assert_eq!(done["remote_verified"], true, "{done}");
    // The read-back version comes back, ready for the next change.
    assert_eq!(done["etag"], "v1", "{done}");
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(body["Event"]["tz"], "UTC");
    assert_eq!(body["Event"]["startDate"], date(2026, 12, 4, 8, 0));
    // A local time still needs a zone, and the text says both ways out.
    let e = error(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"Call","start":"2026-12-04T09:00"}),
    )
    .await;
    assert_eq!(e["error"], "timezone_required");
    let said = e["instructions"].as_str().unwrap();
    assert!(
        said.contains("add timezone") && said.contains("UTC"),
        "{said}"
    );
    // A floating event: offsets keep it in UTC from now on, a local time needs a zone.
    assert_eq!(
        code(
            &mut f.server,
            update("FLOAT", json!({"start":"2026-11-14T07:30"}))
        )
        .await,
        "timezone_required"
    );
    ok(
        &mut f.server,
        update("FLOAT", json!({"start":"2026-11-14T06:30:00Z"})),
    )
    .await;
    let (_, body) = writes(&f.remote).await.pop().unwrap();
    assert_eq!(body["Event"]["tz"], "UTC");
}

#[tokio::test]
async fn the_terminal_text_shows_every_field_and_what_changes() {
    let mut f = fixture(Some("yolo")).await;
    let notes = "n".repeat(250);
    rules(
        f.dir.path(),
        "rules:\n  calendar_write.update: {ask: terminal}\n",
    );
    let id = proposal_id(
        &ok(
            &mut f.server,
            update(
                "E-1",
                json!({"location":"Room 4","notes":notes,"url":"https://example.test/a","title":"Dentist, a long and complete title"}),
            ),
        )
        .await,
    );
    let shown = f.server.approval_text(&id).unwrap();
    for part in [
        "title: Dentist, a long and complete title",
        "location: Room 4",
        "(50 more characters)",
        "url: https://example.test/a",
        "when: unchanged",
    ] {
        assert!(shown.contains(part), "{part}: {shown}");
    }
    // A removed field says so.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut e = library.event("E-1").unwrap().unwrap();
        e.location = Some("Old room".into());
        library.put_events(&[e]).unwrap();
    }
    let id = proposal_id(&ok(&mut f.server, update("E-1", json!({"location":""}))).await);
    let shown = f.server.approval_text(&id).unwrap();
    assert!(shown.contains("location: removed"), "{shown}");
    assert!(shown.contains("Location: Old room"), "{shown}");
    // The whole notes text is bound: a change past what is shown makes it stale.
    let details: Value = serde_json::from_str(&f.server.approval_details(&id).unwrap()).unwrap();
    assert!(details["preview"]["event"]["notes"].is_null(), "{details}");
    let id = proposal_id(&ok(&mut f.server, update("E-1", json!({"title":"t2"}))).await);
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut e = library.event("E-1").unwrap().unwrap();
        e.notes = Some(format!("{}x", "n".repeat(300)));
        library.put_events(&[e]).unwrap();
    }
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("approval_stale_calendar_changed_ask_again")
    );
    // A move card has no alerts line, nor a delete card.
    for args in [
        json!({"action":"move","guid":"E-1","etag":"etag-E-1","calendar_guid":"cal-home"}),
        json!({"action":"delete","guid":"E-1","etag":"etag-E-1"}),
    ] {
        rules(f.dir.path(), "rules:\n  calendar_write: {ask: terminal}\n");
        let id = proposal_id(&ok(&mut f.server, args.clone()).await);
        assert!(
            !f.server.approval_text(&id).unwrap().contains("Alerts"),
            "{args}"
        );
    }
}

#[tokio::test]
async fn the_create_card_shows_the_resolved_event_and_the_invitation_rule_in_force() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        json!({"action":"create","calendar_guid":"cal-home","title":"Lunch","start":"2026-12-05T12:00","timezone":"Europe/Berlin","alerts":[15]}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    for part in [
        "Home",
        "2026-12-05T12:00:00+01:00",
        "one hour after the start",
        "15",
        "members",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert!(!message.contains("nobody is invited"), "{message}");
    f.server.handle(answer(&form, "deny")).await;
    // The invitations line says what the rule in force does.
    let form = call(&mut f.server, update("MEET", json!({"title":"P2"}))).await;
    let message = form["params"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("is refused"), "{message}");
    f.server.handle(answer(&form, "deny")).await;
    rules(
        f.dir.path(),
        "rules:\n  calendar_write.send_invitations: allow\n",
    );
    let form = call(&mut f.server, update("MEET", json!({"title":"P2"}))).await;
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("Apple emails each of them"), "{message}");
    assert!(write_paths(&f.remote).await.is_empty());
}

#[tokio::test]
async fn a_tool_wide_allow_is_not_reported_as_allowing_invitations() {
    let f = fixture(Some("writes_only")).await;
    rules(f.dir.path(), "rules:\n  calendar_write: allow\n");
    let shown =
        skykeep_mcp::permissions::show(&f.dir.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        !shown.contains("looser: calendar_write.send_invitations"),
        "{shown}"
    );
    assert!(
        shown.contains("changes that would notify invited people stay refused"),
        "{shown}"
    );
    let starter = skykeep_mcp::permissions::starter(Some("writes_only"), Path::new("/srv/sky"));
    assert!(
        starter.contains("calendar_write.send_invitations: takes allow or deny only"),
        "{starter}"
    );
}

#[tokio::test]
async fn an_occurrence_answered_only_by_its_series_is_not_verified() {
    let mut f = fixture(Some("yolo")).await;
    *f.fake.master_only.lock().unwrap() = true;
    let done = ok(
        &mut f.server,
        update(
            OCCURRENCE,
            json!({"title":"Standup (once)","scope":"this_occurrence"}),
        ),
    )
    .await;
    assert_eq!(done["remote_verified"], false, "{done}");
    assert!(done.get("etag").is_none(), "{done}");
    assert_eq!(cached(f.dir.path(), OCCURRENCE).unwrap().etag, None);
}

#[tokio::test]
async fn a_whole_series_update_forgets_the_cached_occurrences() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        update(
            OCCURRENCE,
            json!({"title":"Standup (all)","scope":"whole_series"}),
        ),
    )
    .await;
    assert_eq!(done["remote_verified"], false, "{done}");
    assert_eq!(
        write_paths(&f.remote).await,
        [format!("/ca/events/cal-work/{OCCURRENCE}/all")]
    );
    assert!(cached(f.dir.path(), OCCURRENCE).is_none());
}

#[tokio::test]
async fn unchanged_time_fields_are_not_a_time_change_and_errors_name_the_fix() {
    let mut f = fixture(Some("yolo")).await;
    // all_day as it is, and the event's own zone, need no start.
    ok(
        &mut f.server,
        update("A-1", json!({"all_day":true,"title":"Holiday!"})),
    )
    .await;
    ok(
        &mut f.server,
        update(
            "E-1",
            json!({"timezone":"Europe/Berlin","title":"Dentist!"}),
        ),
    )
    .await;
    let mut fresh = fixture(Some("yolo")).await;
    assert_eq!(
        code(
            &mut fresh.server,
            update("E-1", json!({"timezone":"Europe/Berlin"}))
        )
        .await,
        "nothing_to_change"
    );
    let e = error(&mut fresh.server, update("E-1", json!({"all_day":true}))).await;
    assert_eq!(e["error"], "start_required_to_change_all_day");
    assert!(e["instructions"]
        .as_str()
        .unwrap()
        .contains("all_day: true"));
    let e = error(
        &mut fresh.server,
        json!({"action":"create","calendar_guid":"cal-work","title":"x","end":"2026-12-01T10:00Z"}),
    )
    .await;
    assert_eq!(e["error"], "start_required");
    assert!(e["instructions"]
        .as_str()
        .unwrap()
        .contains("2026-03-29T09:30:00+02:00"));
    for (args, why, hint) in [
        (
            json!({"action":"create","calendar_guid":"cal-work","title":"x","start":"2026-12-01T10:00Z","guid":"E-1"}),
            "create_takes_no_guid_etag_or_scope",
            "update",
        ),
        (
            json!({"action":"delete","guid":"E-1","etag":"etag-E-1","title":"x"}),
            "delete_takes_only_guid_etag_and_scope",
            "scope",
        ),
        (
            json!({"action":"create","calendar_guid":"cal-none","title":"x","start":"2026-12-01T10:00Z"}),
            "calendar_not_found",
            "calendar_calendars",
        ),
        (
            update("NOPE", json!({"title":"x"})),
            "event_not_found_run_calendar_refresh",
            "calendar_get",
        ),
        (
            json!({"action":"move","guid":OCCURRENCE,"etag":format!("etag-{OCCURRENCE}"),"calendar_guid":"cal-home"}),
            "move_needs_scope_whole_series",
            "scope: whole_series",
        ),
        (
            update("E-1", json!({"title":"x","scope":"whole_series"})),
            "not_a_repeating_event_leave_out_scope",
            "without scope",
        ),
    ] {
        let e = error(&mut fresh.server, args.clone()).await;
        assert_eq!(e["error"], why, "{args}");
        assert!(
            e["instructions"].as_str().unwrap().contains(hint),
            "{args}: {e}"
        );
    }
    assert!(write_paths(&fresh.remote).await.is_empty());
}
