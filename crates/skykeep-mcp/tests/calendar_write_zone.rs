//! An all-day Calendar change on a computer whose zone skips a midnight. Its own test binary,
//! so the zone is set before anything reads it. Every fixture is synthetic and every remote a
//! loopback mock.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::calendar_write::{Seam, WriteLimits};
use skykeep_mcp::Server;
use std::path::Path;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DSID: &str = "1234567890";

/// Keeps the one event saved, and answers `eventdetail` with it.
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Option<Value>>>);

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut held = self.0.lock().unwrap();
        if request.url.path().starts_with("/ca/eventdetail/") {
            return match &*held {
                Some(e) => ResponseTemplate::new(200).set_body_json(json!({"Event":[e]})),
                None => ResponseTemplate::new(404),
            };
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut event = body["Event"].clone();
        event["etag"] = json!("v1");
        *held = Some(event.clone());
        ResponseTemplate::new(200).set_body_json(json!({"Event":[event]}))
    }
}

#[tokio::test]
async fn an_all_day_event_on_a_skipped_midnight_keeps_its_day() {
    // Set before any zone is read in this process. Chile's clocks go from 2026-09-05 24:00
    // straight to 2026-09-06 01:00: that day has no midnight.
    std::env::set_var("TZ", "America/Santiago");
    // Files the synthetic session store writes are private, as in production.
    unsafe {
        libc::umask(0o077);
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"calendar":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let fake = Fake::default();
    Mock::given(path_regex("^/ca/(eventdetail|events)/"))
        .respond_with(fake.clone())
        .mount(&remote)
        .await;
    let mut server = Server::open(dir.path()).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
        let snapshot = quince_icloud::calendar::parse_snapshot(
            &json!({"Collection":[{"guid":"cal-work","title":"Work","ctag":"c1"}],"Event":[]}),
        );
        quince_icloud::sync::apply_calendar(&mut library, snapshot).unwrap();
    }
    server.set_policy("yolo").unwrap();
    let uri = remote.uri();
    server.set_calendar_seam(Seam {
        client: Some(Arc::new(move |dir: &Path| {
            let store = Store::file(dir).unwrap().without_passwords();
            store
                .save(&Session {
                    dsid: Some(DSID.into()),
                    session_token: Some("SYNTHETIC-TOKEN".into()),
                    webservices: Some(json!({"calendar":{"url":uri}})),
                    ..Default::default()
                })
                .unwrap();
            Ok(Client::with(dir, Endpoints::at(&uri), store).unwrap())
        })),
        limits: WriteLimits::default(),
    });
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"calendar_write","arguments":
            {"action":"create","calendar_guid":"cal-work","title":"Fiestas","all_day":true,"start":"2026-09-06","end":"2026-09-07"}}}))
        .await
        .unwrap();
    let done: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(done["remote_verified"], true, "{done}");
    let saved = fake.0.lock().unwrap().clone().unwrap();
    assert_eq!(saved["startDate"][0], 20_260_906, "{saved}");
    assert_eq!(saved["endDate"][0], 20_260_907, "{saved}");
    // Read back as the day it is, as calendar_get reports it.
    let guid = done["event"]["guid"].as_str().unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"calendar_get","arguments":{"guid":guid}}}))
        .await
        .unwrap();
    let got: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(got["event"]["start"], json!({"date":"2026-09-06"}), "{got}");
    assert_eq!(got["event"]["end"], json!({"date":"2026-09-07"}), "{got}");
}
