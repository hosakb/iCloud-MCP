//! Apple iCloud Photos changes through `photos_write`: each action against a loopback mock of
//! CloudKit and the upload service, the refusal when Apple's copy moved on, the exchange
//! folder's limits, and how approvals and the permissions file decide. Every fixture is
//! synthetic; nothing reaches the network.
use quince_auth::{session::Session, Client, Endpoints, Store};
use quince_core::photos::{Album, Kind, Photo};
use serde_json::{json, Value};
use skykeep_mcp::drive_write::{Seam, WriteLimits};
use skykeep_mcp::Server;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DSID: &str = "1234567890";
const DB: &str = "/database/1/com.apple.photos.cloud/production/private";
const TAG: &str = "tag-1";
const ALBUM_TAG: &str = "album-tag-1";

fn private_dir() -> tempfile::TempDir {
    unsafe {
        libc::umask(0o077);
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn photo(id: &str) -> Photo {
    Photo {
        id: id.into(),
        master: format!("M-{id}"),
        taken_at: 1_700_000_000,
        added_at: Some(1_700_000_060),
        kind: Kind::Image,
        width: 4032,
        height: 3024,
        duration: None,
        filename: Some(format!("{id}.HEIC")),
        size: Some(2_000_000),
        uti: Some("public.heic".into()),
        favorite: false,
        hidden: false,
        deleted: false,
        deleted_at: None,
        expunge_at: None,
        title: None,
        caption: None,
        rating: 0,
        keywords: vec!["Beach".into()],
        latitude: None,
        longitude: None,
        rendered: 0,
        subtype: 0,
        burst: None,
        burst_pick: false,
        burst_count: 0,
        live_size: None,
        change_tag: Some(TAG.into()),
    }
}

fn webservices(uri: &str) -> Value {
    json!({"ckdatabasews":{"url":uri},"photosupload":{"url":uri}})
}

fn client(dir: &Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(webservices(uri)),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

/// The change tag of each record as the mock of Apple holds it: what the cache was given,
/// until a change moves it on.
type Tags = Arc<Mutex<HashMap<String, String>>>;

/// CloudKit's lookup, answering each record at its tag now, or at the fixture's.
struct Lookup(Tags, &'static str);
impl Respond for Lookup {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let name = body["records"][0]["recordName"]
            .as_str()
            .unwrap()
            .to_string();
        let tag = self.0.lock().unwrap().get(&name).cloned();
        let tag = tag.unwrap_or_else(|| self.1.to_string());
        ResponseTemplate::new(200).set_body_json(
            json!({"records":[{"recordName":name,"recordType":"CPLAsset","recordChangeTag":tag,"fields":{}}]}),
        )
    }
}

/// CloudKit's modify, answering the record as sent, at a new change tag.
struct Modify(Tags);
impl Respond for Modify {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut record = body["operations"][0]["record"].clone();
        if record.get("recordName").is_none() {
            record["recordName"] = json!("ALBUM-NEW");
        }
        let mut tags = self.0.lock().unwrap();
        let tag = format!("tag-{}", tags.len() + 2);
        tags.insert(record["recordName"].as_str().unwrap().into(), tag.clone());
        record["recordChangeTag"] = json!(tag);
        ResponseTemplate::new(200).set_body_json(json!({"records":[record]}))
    }
}

/// The upload service's first step, naming the address for the key it was asked about.
struct UploadUrl(String);
impl Respond for UploadUrl {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let key = body["assets"].as_object().unwrap().keys().next().unwrap();
        ResponseTemplate::new(200)
            .set_body_json(json!({"uploadUrls":{key: format!("{}/bytes", self.0)}}))
    }
}

async fn apple(lookup_tag: &'static str) -> MockServer {
    let remote = MockServer::start().await;
    let uri = remote.uri();
    let tags = Tags::default();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"dsInfo":{"dsid":DSID},"webservices":webservices(&uri)})),
        )
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/zones/list")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"zones":[{"zoneID":{"zoneName":"PrimarySync"}}]})),
        )
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/lookup")))
        .respond_with(Lookup(tags.clone(), lookup_tag))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/modify")))
        .respond_with(Modify(tags))
        .mount(&remote)
        .await;
    remote
}

struct Fixture {
    dir: tempfile::TempDir,
    exchange: PathBuf,
    server: Server,
    remote: MockServer,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.exchange);
    }
}

async fn fixture_at(policy: Option<&str>, lookup_tag: &'static str) -> Fixture {
    let remote = apple(lookup_tag).await;
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let exchange = dir.path().parent().unwrap().join(format!(
        "exchange-{}",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir(&exchange).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
        let mut hidden = photo("P-HIDDEN");
        hidden.hidden = true;
        library
            .put_photos(&[photo("P-1"), photo("P-2"), hidden])
            .unwrap();
        for (id, name) in [("AL-1", "Trips"), ("AL-2", "Family")] {
            library
                .put_album(&Album {
                    id: id.into(),
                    name: name.into(),
                    position: 100,
                    change_tag: Some(ALBUM_TAG.into()),
                    folder: false,
                    parent: None,
                })
                .unwrap();
        }
        skykeep_mcp::exchange::set_up(&mut library, Some(exchange.clone()), Some(1)).unwrap();
    }
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    let uri = remote.uri();
    server.set_drive_seam(Seam {
        client: Some(Arc::new(move |dir: &Path| Ok(client(dir, &uri)))),
        limits: WriteLimits {
            change_timeout: Duration::from_secs(20),
            transfer_timeout: Some(Duration::from_secs(30)),
            ..WriteLimits::default()
        },
    });
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}}))
        .await
        .unwrap();
    Fixture {
        dir,
        exchange,
        server,
        remote,
    }
}

async fn fixture(policy: Option<&str>) -> Fixture {
    fixture_at(policy, TAG).await
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn call(server: &mut Server, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"photos_write","arguments":args}}))
        .await
        .unwrap()
}

async fn ok(server: &mut Server, args: Value) -> Value {
    let reply = call(server, args).await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    text(&reply)
}

async fn error(server: &mut Server, args: Value) -> String {
    let reply = call(server, args).await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    text(&reply)["error"].as_str().unwrap().to_string()
}

/// The records Apple was asked to change, as sent.
async fn modified(remote: &MockServer) -> Vec<Value> {
    remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/records/modify"))
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["operations"][0].clone())
        .collect()
}

/// Every request that is not the sign-in check or a zone list.
async fn asked(remote: &MockServer) -> Vec<String> {
    remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p != "/setup/ws/1/validate" && !p.ends_with("/zones/list"))
        .collect()
}

fn cached(dir: &Path, id: &str) -> Photo {
    quince_core::Library::open(dir)
        .unwrap()
        .photo(id)
        .unwrap()
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

fn decide(form: &Value, decision: &str) -> Value {
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":decision}}})
}

fn proposal_id(value: &Value) -> String {
    assert_eq!(value["status"], "approval_required", "{value}");
    value["request_id"].as_str().unwrap().to_string()
}

fn b64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

#[tokio::test]
async fn the_tool_is_listed_with_a_strict_schema_and_its_rule_names() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "photos_write")
        .unwrap()
        .clone();
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        tool["inputSchema"]["properties"]["action"]["enum"],
        json!(skykeep_mcp::photos_write::ACTIONS)
    );
    assert_eq!(tool["annotations"]["destructiveHint"], true);
    let names = skykeep_mcp::permissions::names();
    for action in skykeep_mcp::photos_write::ACTIONS {
        assert!(
            names.contains(&format!("photos_write.{action}")),
            "{action}"
        );
    }
    // The starter file asks before the one removal, as for every tool's deletions.
    let starter = skykeep_mcp::permissions::parse(&skykeep_mcp::permissions::starter(
        Some("writes_only"),
        Path::new("/srv/sky"),
    ))
    .unwrap();
    assert!(starter.contains_key("photos_write.trash"));
    assert!(!starter.contains_key("photos_write.set_favorite"));
}

#[tokio::test]
async fn each_detail_change_is_sent_against_the_cached_version_and_cached_after() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"set_favorite","id":"P-1","favorite":true}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], false);
    let sent = modified(&f.remote).await;
    assert_eq!(sent[0]["record"]["recordName"], "P-1");
    assert_eq!(sent[0]["record"]["recordChangeTag"], TAG);
    assert_eq!(
        sent[0]["record"]["fields"],
        json!({"isFavorite":{"value":1}})
    );
    let after = cached(f.dir.path(), "P-1");
    assert!(after.favorite);
    assert_eq!(after.change_tag.as_deref(), Some("tag-2"));

    // Each further change works on a photo still at the cached version.
    let mut g = fixture(Some("yolo")).await;
    let cases = [
        (
            json!({"action":"set_rating","id":"P-1","rating":4}),
            json!({"rating":{"value":4}}),
        ),
        (
            json!({"action":"set_date","id":"P-2","taken_at":"2024-05-01T14:30:00+02:00"}),
            json!({"assetDate":{"value":1_714_566_600_000_i64}}),
        ),
    ];
    for (args, fields) in cases {
        let done = ok(&mut g.server, args.clone()).await;
        assert_eq!(done["outcome"], "done", "{args}: {done}");
        let sent = modified(&g.remote).await;
        assert_eq!(sent.last().unwrap()["record"]["fields"], fields, "{args}");
        assert_eq!(
            sent.last().unwrap()["record"]["recordChangeTag"],
            TAG,
            "{args}"
        );
    }
    assert_eq!(cached(g.dir.path(), "P-1").rating, 4);
    assert_eq!(cached(g.dir.path(), "P-2").taken_at, 1_714_566_600);
}

#[tokio::test]
async fn title_caption_and_keywords_send_only_what_changed() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"set_title_and_caption","id":"P-1","title":"Strand é","caption":""}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    let sent = modified(&f.remote).await;
    assert_eq!(
        sent[0]["record"]["fields"],
        json!({"captionEnc":{"value":b64("Strand é")},"extendedDescEnc":{"value":""}})
    );
    let after = cached(f.dir.path(), "P-1");
    assert_eq!(after.title.as_deref(), Some("Strand é"));
    assert_eq!(after.caption, None);

    let mut g = fixture(Some("yolo")).await;
    let done = ok(
        &mut g.server,
        json!({"action":"set_keywords","id":"P-2","keywords":["Sea","Summer"]}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["keywords"], json!(["Sea", "Summer"]));
    let sent = modified(&g.remote).await;
    assert!(sent[0]["record"]["fields"]["keywordsEnc"]["value"].is_string());
    assert_eq!(cached(g.dir.path(), "P-2").keywords, ["Sea", "Summer"]);
}

#[tokio::test]
async fn a_photo_or_album_changed_at_apple_is_refused_not_written_over() {
    let mut f = fixture_at(Some("yolo"), "tag-moved").await;
    for args in [
        json!({"action":"set_favorite","id":"P-1","favorite":true}),
        json!({"action":"set_title_and_caption","id":"P-1","title":"x"}),
        json!({"action":"set_keywords","id":"P-1","keywords":[]}),
        json!({"action":"set_rating","id":"P-1","rating":1}),
        json!({"action":"set_date","id":"P-1","taken_at":"2020-01-01T00:00:00Z"}),
        json!({"action":"rename_album","album_id":"AL-1","name":"Holidays"}),
    ] {
        assert_eq!(
            error(&mut f.server, args.clone()).await,
            "photos_item_changed_run_photos_refresh",
            "{args}"
        );
    }
    assert!(modified(&f.remote).await.is_empty());
    let reply = call(
        &mut f.server,
        json!({"action":"set_rating","id":"P-1","rating":1}),
    )
    .await;
    assert!(text(&reply)["instructions"]
        .as_str()
        .unwrap()
        .contains("photos_refresh"));
    // The cache keeps what it had.
    assert_eq!(cached(f.dir.path(), "P-1").change_tag.as_deref(), Some(TAG));
}

#[tokio::test]
async fn hidden_unknown_and_unversioned_photos_are_refused_before_apple_is_asked() {
    let mut f = fixture(Some("yolo")).await;
    for (id, code) in [("P-HIDDEN", "not_found"), ("P-NONE", "not_found")] {
        assert_eq!(
            error(
                &mut f.server,
                json!({"action":"set_favorite","id":id,"favorite":true})
            )
            .await,
            code
        );
    }
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut p = photo("P-2");
        p.change_tag = None;
        library.put_photos(&[p]).unwrap();
    }
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"set_rating","id":"P-2","rating":2})
        )
        .await,
        "photos_item_version_unknown_run_photos_refresh"
    );
    assert!(modified(&f.remote).await.is_empty());
}

#[tokio::test]
async fn albums_are_created_and_renamed_and_a_taken_name_is_refused() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"create_album","name":"Hiking"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["album"]["id"], "ALBUM-NEW");
    let sent = modified(&f.remote).await;
    assert_eq!(sent[0]["operationType"], "create");
    assert_eq!(
        sent[0]["record"]["fields"]["albumNameEnc"]["value"],
        b64("Hiking")
    );
    // It sorts first: two below the first album there.
    assert_eq!(sent[0]["record"]["fields"]["position"]["value"], 98);
    for (args, code) in [
        (
            json!({"action":"create_album","name":"trips"}),
            "album_name_taken",
        ),
        (
            json!({"action":"rename_album","album_id":"AL-1","name":"Family"}),
            "album_name_taken",
        ),
        (
            json!({"action":"rename_album","album_id":"AL-1","name":"Trips"}),
            "album_already_has_this_name",
        ),
        (
            json!({"action":"rename_album","album_id":"AL-9","name":"X"}),
            "album_not_found",
        ),
    ] {
        assert_eq!(error(&mut f.server, args.clone()).await, code, "{args}");
    }
    let mut g = fixture_at(Some("yolo"), ALBUM_TAG).await;
    let done = ok(
        &mut g.server,
        json!({"action":"rename_album","album_id":"AL-1","name":"Holidays"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    let sent = modified(&g.remote).await;
    assert_eq!(sent[0]["record"]["recordChangeTag"], ALBUM_TAG);
    let albums = quince_core::Library::open(g.dir.path())
        .unwrap()
        .albums()
        .unwrap();
    assert!(albums
        .iter()
        .any(|a| a.id == "AL-1" && a.name == "Holidays"));
}

fn jpeg(at: &Path, bytes: usize) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(at, vec![0xAB; bytes]).unwrap();
    std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o600)).unwrap();
}

async fn serve_upload(remote: &MockServer) {
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/photosupload/createUploadUrl"))
        .respond_with(UploadUrl(uri))
        .mount(remote)
        .await;
    Mock::given(method("POST"))
        .and(path("/bytes"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"singleFile":{"receipt":"R","fileChecksum":"C"}})),
        )
        .mount(remote)
        .await;
    Mock::given(method("POST"))
        .and(path("/photosupload/putAsset"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"uploadJobId":"J1","cplAsset":"A-NEW","cplMaster":"M-NEW"}]),
            ),
        )
        .mount(remote)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/photosupload/uploadStatus$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"J1":{"progress":100.0}})))
        .mount(remote)
        .await;
}

#[tokio::test]
async fn an_upload_sends_exactly_the_exchange_file_and_waits_for_apples_job() {
    let mut f = fixture(Some("yolo")).await;
    jpeg(&f.exchange.join("IMG_0001.JPG"), 4096);
    serve_upload(&f.remote).await;
    let done = ok(
        &mut f.server,
        json!({"action":"upload","local":"IMG_0001.JPG"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["photo"]["id"], "A-NEW");
    assert_eq!(done["remote_verified"], false);
    let requests = f.remote.received_requests().await.unwrap();
    let bytes = requests.iter().find(|r| r.url.path() == "/bytes").unwrap();
    assert_eq!(bytes.body, vec![0xAB; 4096]);
    let put: Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|r| r.url.path() == "/photosupload/putAsset")
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(put["files"][0]["fileName"], "IMG_0001.JPG");
    assert_eq!(put["zoneName"], "PrimarySync");
    // The staged copy is gone.
    let staging = f.dir.path().join("drive-upload");
    assert!(std::fs::read_dir(&staging).map_or(true, |mut d| d.next().is_none()));
}

#[tokio::test]
async fn uploads_refuse_links_escapes_large_files_and_other_types_before_apple_is_asked() {
    let mut f = fixture(Some("yolo")).await;
    let outside = f.dir.path().parent().unwrap().join(format!(
        "outside-{}.jpg",
        f.dir.path().file_name().unwrap().to_string_lossy()
    ));
    jpeg(&outside, 4096);
    std::os::unix::fs::symlink(&outside, f.exchange.join("link.jpg")).unwrap();
    std::fs::create_dir(f.exchange.join("in")).unwrap();
    std::os::unix::fs::symlink(f.dir.path(), f.exchange.join("dirlink")).unwrap();
    jpeg(&f.exchange.join("big.jpg"), 1024 * 1024 + 1);
    jpeg(&f.exchange.join("tiny.jpg"), 10);
    jpeg(&f.exchange.join("notes.txt"), 4096);
    jpeg(&f.exchange.join("hard.jpg"), 4096);
    std::fs::hard_link(
        f.exchange.join("hard.jpg"),
        f.exchange.join("in/second.jpg"),
    )
    .unwrap();
    for (local, code) in [
        ("link.jpg", "local_symlink_refused"),
        ("dirlink/x.jpg", "local_symlink_refused"),
        ("../x.jpg", "local_path_traversal_refused"),
        ("/etc/x.jpg", "absolute_local_path_refused"),
        ("big.jpg", "local_file_over_exchange_limit"),
        ("tiny.jpg", "photos_file_too_small"),
        ("notes.txt", "photos_file_type_not_supported"),
        ("hard.jpg", "local_hard_link_refused"),
        ("missing.jpg", "local_file_not_found"),
    ] {
        assert_eq!(
            error(&mut f.server, json!({"action":"upload","local":local})).await,
            code,
            "{local}"
        );
    }
    let _ = std::fs::remove_file(&outside);
    assert!(asked(&f.remote).await.is_empty());
}

#[tokio::test]
async fn an_upload_without_an_exchange_folder_is_refused() {
    let mut f = fixture(Some("yolo")).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library.set_state("mcp.drive.exchange", "").unwrap();
    }
    let reply = call(&mut f.server, json!({"action":"upload","local":"a.jpg"})).await;
    let value = text(&reply);
    assert_eq!(
        value["error"],
        "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal"
    );
    assert!(value["instructions"]
        .as_str()
        .unwrap()
        .contains("skykeep-mcp exchange"));
}

#[tokio::test]
async fn a_move_to_recently_deleted_always_asks_and_never_offers_always_allow() {
    // Under writes_only: the card, without Always allow, even with an allow rule.
    let mut f = fixture(Some("writes_only")).await;
    for rule in [None, Some("rules:\n  photos_write.trash: allow\n")] {
        if let Some(rule) = rule {
            rules(f.dir.path(), rule);
        }
        let form = call(&mut f.server, json!({"action":"trash","id":"P-1"})).await;
        assert_eq!(form["method"], "elicitation/create", "{rule:?}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"], "{rule:?}");
        f.server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
            .await;
    }
    assert!(modified(&f.remote).await.is_empty());
    // Approving the card runs it once.
    let form = call(&mut f.server, json!({"action":"trash","id":"P-1"})).await;
    let done = text(&f.server.handle(decide(&form, "approve")).await.unwrap());
    assert_eq!(done["outcome"], "done", "{done}");
    let sent = modified(&f.remote).await;
    assert_eq!(
        sent[0]["record"]["fields"],
        json!({"isDeleted":{"value":1}})
    );
    assert!(cached(f.dir.path(), "P-1").deleted);

    // Under yolo and without a mode, even with an allow rule, it waits for the terminal.
    for mode in [Some("yolo"), None] {
        let mut f = fixture(mode).await;
        rules(f.dir.path(), "rules:\n  photos_write: allow\n");
        let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":"P-1"})).await);
        assert!(modified(&f.remote).await.is_empty(), "{mode:?}");
        let done = f.server.approve_any(&id).await.unwrap();
        assert_eq!(done["outcome"], "done", "{mode:?}: {done}");
        assert_eq!(modified(&f.remote).await.len(), 1, "{mode:?}");
        // Used up: it runs once.
        assert!(f.server.approve_any(&id).await.is_err(), "{mode:?}");
        assert!(!f
            .dir
            .path()
            .join(skykeep_mcp::permissions::LOG_NAME)
            .exists());
    }
}

#[tokio::test]
async fn a_detail_change_offers_always_allow_and_the_saved_rule_runs_it_unasked() {
    let mut f = fixture(Some("writes_only")).await;
    let args = json!({"action":"set_favorite","id":"P-1","favorite":true});
    let form = call(&mut f.server, args.clone()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    let done = text(
        &f.server
            .handle(decide(&form, "always_allow"))
            .await
            .unwrap(),
    );
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["permission_rule"]["saved"], true, "{done}");
    let saved =
        std::fs::read_to_string(f.dir.path().join(skykeep_mcp::permissions::FILE_NAME)).unwrap();
    assert!(
        saved.contains("photos_write.set_favorite: allow"),
        "{saved}"
    );
    let again = ok(
        &mut f.server,
        json!({"action":"set_favorite","id":"P-1","favorite":false}),
    )
    .await;
    assert_eq!(again["outcome"], "done", "{again}");
    // Another action still asks.
    let form = call(
        &mut f.server,
        json!({"action":"set_rating","id":"P-1","rating":3}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
}

#[tokio::test]
async fn a_card_goes_stale_when_the_photo_changes_in_the_cache_before_it_is_answered() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        json!({"action":"set_rating","id":"P-1","rating":5}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut p = photo("P-1");
        p.rating = 2;
        library.put_photos(&[p]).unwrap();
    }
    let reply = f.server.handle(decide(&form, "approve")).await.unwrap();
    assert_eq!(text(&reply)["error"], "approval_policy_changed", "{reply}");
    assert!(modified(&f.remote).await.is_empty());
}

#[tokio::test]
async fn deny_rules_refuse_and_a_broken_file_keeps_them() {
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  photos_write.set_rating: deny\n");
    let args = json!({"action":"set_rating","id":"P-1","rating":3});
    assert_eq!(
        error(&mut f.server, args.clone()).await,
        "denied_by_permissions"
    );
    rules(f.dir.path(), "rules:\n  photos_write.set_rating: maybe\n");
    let reply = call(&mut f.server, args.clone()).await;
    let value = text(&reply);
    assert_eq!(value["error"], "denied_by_permissions", "{value}");
    // A terminal request made earlier is refused once the file says deny.
    std::fs::remove_file(f.dir.path().join(skykeep_mcp::permissions::FILE_NAME)).unwrap();
    let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":"P-1"})).await);
    rules(f.dir.path(), "rules:\n  photos_write.trash: deny\n");
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("denied_by_permissions")
    );
    assert!(modified(&f.remote).await.is_empty());
}

#[tokio::test]
async fn a_broken_file_with_no_earlier_rules_still_sends_trash_to_a_person() {
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  bogus_name: allow\n");
    let reply = call(&mut f.server, json!({"action":"trash","id":"P-1"})).await;
    assert_ne!(reply["method"], "elicitation/create", "{reply}");
    proposal_id(&text(&reply));
    assert!(modified(&f.remote).await.is_empty());
}

#[tokio::test]
async fn permissions_show_says_a_photos_trash_still_asks() {
    let f = fixture(Some("writes_only")).await;
    rules(
        f.dir.path(),
        "rules:\n  photos_write.trash: allow\n  photos_write.set_rating: allow\n",
    );
    let shown =
        skykeep_mcp::permissions::show(&f.dir.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        shown.contains("photos_write.trash: allow, but asks in your MCP client, because a move to Recently Deleted in Photos always needs a person"),
        "{shown}"
    );
    assert!(
        shown.contains("photos_write.set_rating: allow, runs without asking"),
        "{shown}"
    );
}

#[tokio::test]
async fn nothing_secret_reaches_a_reply() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"set_favorite","id":"P-1","favorite":true}),
    )
    .await
    .to_string();
    for secret in ["SYNTHETIC-TOKEN", DSID, &f.remote.uri()] {
        assert!(!done.contains(secret), "{secret}: {done}");
    }
}

#[tokio::test]
async fn an_answer_that_does_not_come_in_time_is_an_unknown_outcome_with_what_to_do() {
    let mut f = fixture(Some("yolo")).await;
    f.remote.reset().await;
    let slow = apple(TAG).await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/modify")))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .with_priority(1)
        .mount(&slow)
        .await;
    let uri = slow.uri();
    f.server.set_drive_seam(Seam {
        client: Some(Arc::new(move |dir: &Path| Ok(client(dir, &uri)))),
        limits: WriteLimits {
            change_timeout: Duration::from_secs(1),
            ..WriteLimits::default()
        },
    });
    let reply = call(
        &mut f.server,
        json!({"action":"set_rating","id":"P-1","rating":5}),
    )
    .await;
    let value = text(&reply);
    assert_eq!(
        value["error"],
        "photos_write_outcome_unknown_run_photos_refresh_before_retrying"
    );
    assert!(value["instructions"]
        .as_str()
        .unwrap()
        .contains("may or may not have been made"));
    // The cache is left as it was, for the refresh to settle.
    assert_eq!(cached(f.dir.path(), "P-1").rating, 0);
}

/// A modify answered as CloudKit answers a write over a version another device replaced.
async fn conflict_on_write(remote: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/modify")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"records":[{"recordName":"X","reason":"record changed","serverErrorCode":"CONFLICT"}]}),
        ))
        .with_priority(1)
        .mount(remote)
        .await;
}

#[tokio::test]
async fn a_conflict_at_write_time_is_refused_and_nothing_is_sent_again() {
    // Apple's lookup still shows the cached version, but another device writes first, so
    // CloudKit refuses the write itself.
    for (lookup, args, tag) in [
        (
            TAG,
            json!({"action":"set_favorite","id":"P-1","favorite":true}),
            TAG,
        ),
        (
            TAG,
            json!({"action":"set_title_and_caption","id":"P-1","title":"x"}),
            TAG,
        ),
        (
            TAG,
            json!({"action":"set_rating","id":"P-1","rating":1}),
            TAG,
        ),
        (
            TAG,
            json!({"action":"set_date","id":"P-1","taken_at":"2020-01-01T00:00:00Z"}),
            TAG,
        ),
        (
            ALBUM_TAG,
            json!({"action":"rename_album","album_id":"AL-1","name":"Holidays"}),
            ALBUM_TAG,
        ),
    ] {
        let mut f = fixture_at(Some("yolo"), lookup).await;
        conflict_on_write(&f.remote).await;
        assert_eq!(
            error(&mut f.server, args.clone()).await,
            "photos_item_changed_run_photos_refresh",
            "{args}"
        );
        // One write, over the cached version, never a second one over a newer tag.
        let sent = modified(&f.remote).await;
        assert_eq!(sent.len(), 1, "{args}");
        assert_eq!(sent[0]["operationType"], "update", "{args}");
        assert_eq!(sent[0]["record"]["recordChangeTag"], tag, "{args}");
        assert_eq!(cached(f.dir.path(), "P-1"), photo("P-1"), "{args}");
    }
    // A move to Recently Deleted, approved in the terminal, is refused the same way.
    let mut f = fixture(Some("yolo")).await;
    conflict_on_write(&f.remote).await;
    let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":"P-1"})).await);
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("photos_item_changed_run_photos_refresh")
    );
    assert_eq!(modified(&f.remote).await.len(), 1);
    assert!(!cached(f.dir.path(), "P-1").deleted);
}

#[tokio::test]
async fn an_album_renamed_once_can_be_renamed_again_without_a_refresh() {
    let mut f = fixture_at(Some("yolo"), ALBUM_TAG).await;
    for name in ["Holidays", "Summer"] {
        let done = ok(
            &mut f.server,
            json!({"action":"rename_album","album_id":"AL-1","name":name}),
        )
        .await;
        assert_eq!(done["outcome"], "done", "{name}: {done}");
    }
    let sent = modified(&f.remote).await;
    assert_eq!(sent[1]["record"]["recordChangeTag"], "tag-2");
    assert_eq!(
        sent[1]["record"]["fields"]["albumNameEnc"]["value"],
        b64("Summer")
    );
    assert_eq!(sent[1]["record"]["recordType"], "CPLAlbum");
}

#[tokio::test]
async fn a_terminal_trash_request_shows_plain_lines_and_holds_to_its_preview() {
    let mut f = fixture(Some("yolo")).await;
    // A photo the cache doesn't have is refused at once, with no request made.
    assert_eq!(
        error(&mut f.server, json!({"action":"trash","id":"P-NONE"})).await,
        "not_found"
    );
    let reply = ok(&mut f.server, json!({"action":"trash","id":"P-1"})).await;
    let id = proposal_id(&reply);
    assert!(reply["approve_in_terminal"]
        .as_str()
        .unwrap()
        .contains("skykeep-mcp approve"));
    let shown = f.server.approval_text(&id).unwrap();
    assert!(
        shown.starts_with("Photos change: trash, a move to Recently Deleted\n"),
        "{shown}"
    );
    assert!(
        shown.contains("What it does: moved to Recently Deleted"),
        "{shown}"
    );
    assert!(
        shown.contains("Photo: P-1.HEIC (taken 2023-11-14T22:13:20Z, id P-1)"),
        "{shown}"
    );
    assert!(!shown.contains('{'), "{shown}");
    // The photo changes in the cache before the person approves: the request is stale.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut p = photo("P-1");
        p.title = Some("Renamed".into());
        library.put_photos(&[p]).unwrap();
    }
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("approval_stale_photos_changed_ask_again")
    );
    assert!(modified(&f.remote).await.is_empty());
    let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":"P-1"})).await);
    assert!(f
        .server
        .approval_text(&id)
        .unwrap()
        .contains("title Renamed"));
    let done = f.server.approve_any(&id).await.unwrap();
    assert_eq!(done["outcome"], "done", "{done}");
}

#[tokio::test]
async fn album_and_file_refusals_say_what_to_do() {
    let mut f = fixture(Some("yolo")).await;
    jpeg(&f.exchange.join("tiny.jpg"), 10);
    for (args, words) in [
        (
            json!({"action":"create_album","name":"Trips"}),
            "Ask again with another name",
        ),
        (
            json!({"action":"rename_album","album_id":"AL-1","name":"Trips"}),
            "Nothing to do",
        ),
        (
            json!({"action":"upload","local":"tiny.jpg"}),
            "at least 125 bytes",
        ),
        (
            json!({"action":"upload","local":"notes.txt"}),
            "HEIC (.heic, .heif)",
        ),
    ] {
        let value = text(&call(&mut f.server, args.clone()).await);
        assert!(
            value["instructions"].as_str().unwrap().contains(words),
            "{args}: {value}"
        );
    }
}
