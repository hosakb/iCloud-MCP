//! A Calendar refresh on a computer whose zone is not UTC. Its own test binary, so the zone is
//! set before anything reads it. Every fixture is synthetic and every remote a loopback mock.
use icloud_mcp::Server;
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DSID: &str = "1234567890";

fn date(y: i64, m: i64, d: i64, h: i64, min: i64) -> Value {
    json!([y * 10_000 + m * 100 + d, y, m, d, h, min, h * 60 + min])
}

fn body(with_removed: bool) -> Value {
    let mut events = vec![
        json!({"guid":"KEPT","pGuid":"cal-work","title":"Kept","tz":"UTC","startDate":date(2026,10,15,9,0),"endDate":date(2026,10,15,10,0)}),
    ];
    if with_removed {
        // On the window's first day: an all-day event and a floating one, both read in
        // this computer's zone (UTC+05:30) as instants on the day before.
        events.push(json!({"guid":"DAY-ONE","pGuid":"cal-work","title":"Removed holiday","allDay":true,"startDate":date(2026,10,1,0,0),"endDate":date(2026,10,2,0,0)}));
        // On the first day Apple is asked about (the day before the window): read as an
        // instant in this zone it falls on the day before that, outside what was asked.
        events.push(json!({"guid":"DAY-EDGE","pGuid":"cal-work","title":"Removed edge","allDay":true,"startDate":date(2026,9,30,0,0),"endDate":date(2026,10,1,0,0)}));
        events.push(json!({"guid":"FLOAT-ONE","pGuid":"cal-work","title":"Removed gym","startDate":date(2026,10,1,1,0),"endDate":date(2026,10,1,2,0)}));
    }
    json!({"Collection":[{"guid":"cal-work","title":"Work","ctag":"c1"}],"Event":events})
}

async fn apple(answer: Value) -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"calendar":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    for p in ["/ca/startup", "/ca/events"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
            .mount(&remote)
            .await;
    }
    remote
}

async fn refresh(dir: &std::path::Path, uri: &str) {
    use std::os::unix::fs::PermissionsExt;
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"calendar":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    let client = Client::with(dir, Endpoints::at(uri), store).unwrap();
    icloud_mcp::calendar::refresh_with_client(
        dir,
        client,
        json!({"from":"2026-10-01","to":"2026-11-01"}),
    )
    .await
    .unwrap();
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

async fn october_first(server: &mut Server) -> Value {
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"calendar_events","arguments":{"from":"2026-10-01","to":"2026-10-02","timezone":"Asia/Kolkata"}}}))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn removed_all_day_and_floating_events_on_the_first_day_go_in_any_computer_zone() {
    // Set before any zone is read in this process.
    std::env::set_var("TZ", "Asia/Kolkata");
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();

    refresh(dir.path(), &apple(body(true)).await.uri()).await;
    let first = october_first(&mut server).await;
    let titles: Vec<&str> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["guid"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["DAY-ONE", "FLOAT-ONE"], "{first}");
    assert_eq!(first["items"][0]["start"], json!({"date":"2026-10-01"}));
    assert!(quince_core::Library::open(dir.path())
        .unwrap()
        .event("DAY-EDGE")
        .unwrap()
        .is_some());

    // Apple no longer lists them: a refresh covering that day removes them, as it says.
    refresh(dir.path(), &apple(body(false)).await.uri()).await;
    let first = october_first(&mut server).await;
    assert_eq!(first["coverage"]["range_within_last_refresh"], true);
    assert_eq!(first["items"], json!([]), "{first}");
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert!(library.event("KEPT").unwrap().is_some());
    assert!(library.event("DAY-ONE").unwrap().is_none());
    assert!(library.event("FLOAT-ONE").unwrap().is_none());
    assert!(library.event("DAY-EDGE").unwrap().is_none());
}
