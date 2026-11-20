//! Apple iCloud Drive, read-only metadata: a cached folder listing, one item's details, a
//! name search, and an explicit refresh that reads one folder (and each folder on its path)
//! into the cache. No file contents, no writes. Every fixture is synthetic and every remote
//! is a loopback mock.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::Server;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DSID: &str = "1234567890";
const ROOT: &str = "FOLDER::com.apple.CloudDocs::root";
const DOCS: &str = "FOLDER::com.apple.CloudDocs::DOCS";
const DEEP: &str = "FOLDER::com.apple.CloudDocs::DEEP";
const PAGES: &str = "FOLDER::com.apple.Pages::documents";
const DEEP_NAME: &str = "Ünïcödé 📁";

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn folder(id: &str, name: &str) -> Value {
    json!({"drivewsid":id,"name":name,"type":"FOLDER","directChildrenCount":2,"etag":"e1",
        "dateCreated":"2026-01-02T03:04:05Z","dateModified":"2026-02-03T04:05:06Z"})
}

fn file(id: &str, name: &str, ext: &str, size: i64) -> Value {
    json!({"drivewsid":id,"name":name,"extension":ext,"type":"FILE","size":size,"etag":"e2",
        "dateModified":"2026-03-04T05:06:07Z"})
}

fn listing(id: &str, items: Value) -> Value {
    json!([{"drivewsid":id,"status":"OK","items":items}])
}

fn root_items() -> Value {
    json!([
        file("FILE::com.apple.CloudDocs::REPORT", "report", "pdf", 20480),
        folder(DOCS, "Documents"),
        {"drivewsid":PAGES,"name":"Pages","type":"APP_LIBRARY","size":-1},
        // A Pages, Numbers or Keynote document is a package Apple lists as a file.
        file("FILE::com.apple.CloudDocs::BUDGET", "Budget", "numbers", 4096),
        // A type this server does not know: listed, never opened as a folder.
        {"drivewsid":"FILE::com.apple.CloudDocs::ODD","name":"Odd","type":"PACKAGE"},
        // Recently Deleted items never show.
        {"drivewsid":"FILE::com.apple.CloudDocs::GONE","name":"gone","extension":"txt",
            "type":"FILE","restorePath":"/Documents","dateExpiration":"2026-12-01T00:00:00Z"},
        {"drivewsid":"FILE::com.apple.CloudDocs::GONE2","name":"gone too","type":"FILE",
            "parentId":"TRASH_ROOT"}
    ])
}

fn docs_items() -> Value {
    json!([
        folder(DEEP, DEEP_NAME),
        file("FILE::com.apple.CloudDocs::S10", "Scan 10", "jpg", 10),
        file("FILE::com.apple.CloudDocs::S9", "Scan 9", "jpg", 9),
        file("FILE::com.apple.CloudDocs::S11", "Scan 11", "jpg", 11),
        file("FILE::com.apple.CloudDocs::ETE", "Été", "txt", 1),
    ])
}

fn deep_items() -> Value {
    json!([
        file("FILE::com.apple.CloudDocs::BELL", "bell\u{7}name", "txt", 1),
        file(
            "FILE::com.apple.CloudDocs::LONG",
            &"L".repeat(3000),
            "txt",
            1
        ),
        file("FILE::com.apple.CloudDocs::SLASH", "a/b", "txt", 1),
        file(
            &format!("FILE::com.apple.CloudDocs::{}", "I".repeat(600)),
            "long id",
            "txt",
            1
        ),
    ])
}

async fn apple() -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"drivews":{"url":remote.uri()},"ckdatabasews":{"url":remote.uri()},
                "contacts":{"url":remote.uri()},"calendar":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    remote
}

async fn serve(remote: &MockServer, id: &str, answer: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/retrieveItemDetailsInFolders"))
        .and(body_json(json!([{"drivewsid":id,"partialData":false}])))
        .respond_with(answer)
        .mount(remote)
        .await;
}

async fn serve_tree(remote: &MockServer) {
    for (id, items) in [
        (ROOT, root_items()),
        (DOCS, docs_items()),
        (DEEP, deep_items()),
    ] {
        serve(
            remote,
            id,
            ResponseTemplate::new(200).set_body_json(listing(id, items)),
        )
        .await;
    }
}

fn saved_client_with(dir: &std::path::Path, uri: &str, adp: bool) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"drivews":{"url":uri},"ckdatabasews":{"url":uri},
                "contacts":{"url":uri},"calendar":{"url":uri}})),
            data_protection_seen: adp.then_some(true),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
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
        skykeep_mcp::drive::refresh_with_client(dir, saved_client_with(dir, uri, false), args)
            .await;
    private_files(dir);
    result
}

async fn refresh_limited(
    dir: &std::path::Path,
    uri: &str,
    args: Value,
    limits: skykeep_mcp::drive::RefreshLimits,
) -> Result<Value, &'static str> {
    let result = skykeep_mcp::drive::refresh_with_client_limited(
        dir,
        saved_client_with(dir, uri, false),
        args,
        limits,
    )
    .await;
    private_files(dir);
    result
}

/// A bound directory with unrelated Notes, Reminders, Contacts and Calendar state and queued
/// writes of every service, Drive's included.
fn bound(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    for service in ["notes", "reminders", "contacts", "calendar"] {
        library
            .mark_synced(service, Some(&format!("{service}-CURSOR")))
            .unwrap();
    }
    library.mark_synced("drive", Some("DRIVE-CURSOR")).unwrap();
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    for (service, record) in [
        ("notes", "N-1"),
        ("reminders", "R-1"),
        ("contacts", "C-1"),
        ("calendar", "E-1"),
        ("drive", "FILE::com.apple.CloudDocs::REPORT"),
    ] {
        library
            .outbox_append(service, record, "delete", "{\"queued\":true}")
            .unwrap();
    }
    (dir, server)
}

/// Everything a Drive refresh must leave alone: queued writes, other services' cursors, the
/// policy and Drive's own cursor.
fn untouched(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let pending: Vec<Value> = library
        .outbox_all_pending()
        .unwrap()
        .into_iter()
        .map(|p| json!([p.id, p.service, p.record, p.kind, p.payload, p.tries]))
        .collect();
    let mut states = Vec::new();
    for service in ["notes", "reminders", "contacts", "calendar"] {
        states.push(json!([
            library.sync_token(service).unwrap(),
            library.last_synced(service).unwrap(),
        ]));
    }
    json!({
        "pending": pending,
        "refused": library.outbox_refused_count().unwrap(),
        "states": states,
        "drive_cursor": library.sync_token("drive").unwrap(),
        "policy": library.state("mcp.policy").unwrap(),
    })
}

/// The Drive cache: every row under the folders the fixture knows, and the listed marks.
fn cached(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let mut rows = Vec::new();
    for id in [ROOT, DOCS, DEEP, PAGES] {
        rows.push(json!([id, library.drive_children(id).unwrap()]));
    }
    json!({"rows": rows, "listed": library.state("mcp.drive.listed").unwrap(),
        "cursor": library.sync_token("drive").unwrap()})
}

fn drive_failed(dir: &std::path::Path) -> bool {
    quince_core::Library::open(dir)
        .unwrap()
        .sync_health()
        .unwrap()
        .iter()
        .any(|h| h.service == "drive" && h.last_error.is_some())
}

/// Only Drive folder listings: a POST to `retrieveItemDetailsInFolders` for one folder.
fn only_drive_listings(requests: &[wiremock::Request]) -> Vec<String> {
    let mut asked = Vec::new();
    for r in requests {
        let p = r.url.path();
        if p == "/setup/ws/1/validate" {
            continue;
        }
        assert_eq!(r.method.as_str(), "POST", "unexpected {} {p}", r.method);
        assert_eq!(p, "/retrieveItemDetailsInFolders", "unexpected {p}");
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        let ask = body.as_array().unwrap();
        assert_eq!(ask.len(), 1, "{body}");
        assert_eq!(ask[0]["partialData"], false);
        asked.push(ask[0]["drivewsid"].as_str().unwrap().to_string());
    }
    asked
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

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap_or("<null>").to_string())
        .collect()
}

fn names(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["display_name"].as_str().unwrap_or("<null>").to_string())
        .collect()
}

/// A bound directory whose cache holds the whole synthetic tree, read by refresh before the
/// policy is set (the test seam is never approved under `reads_and_writes`).
async fn refreshed(policy: Option<&str>) -> (tempfile::TempDir, Server, MockServer) {
    let remote = apple().await;
    serve_tree(&remote).await;
    let (dir, mut server) = bound(None);
    refresh(
        dir.path(),
        &remote.uri(),
        json!({"path":format!("/Documents/{DEEP_NAME}")}),
    )
    .await
    .unwrap();
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    (dir, server, remote)
}

const NAMES: [&str; 4] = ["drive_list", "drive_get", "drive_search", "drive_refresh"];

#[tokio::test]
async fn tools_list_names_apple_icloud_drive_with_strict_read_only_schemas() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let init = server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Apple iCloud Drive"),
        "{instructions}"
    );
    assert!(instructions.contains("drive_refresh"), "{instructions}");
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let drive: Vec<&Value> = tools
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with("drive"))
        .collect();
    let found: Vec<&str> = drive.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(found, NAMES);
    for tool in &drive {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("Apple iCloud Drive"), "{name}");
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        let hints = &tool["annotations"];
        assert_eq!(hints["destructiveHint"], false, "{name}");
        if name == "drive_refresh" {
            assert_eq!(hints["readOnlyHint"], false, "refresh replaces the cache");
            assert_eq!(hints["openWorldHint"], true, "refresh reads from Apple");
        } else {
            assert_eq!(hints["readOnlyHint"], true, "{name}");
            assert_eq!(hints["openWorldHint"], false, "{name}");
        }
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        for key in props.keys() {
            assert!(
                ["path", "id", "query", "offset", "limit"].contains(&key.as_str()),
                "{name}: {key}"
            );
        }
    }
    assert_eq!(
        drive[0]["inputSchema"]["properties"]["limit"]["maximum"],
        100
    );
    assert_eq!(drive[2]["inputSchema"]["required"], json!(["query"]));
    // No tool downloads, uploads, renames, moves or deletes Drive items.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for word in [
            "download", "upload", "rename", "move", "delete", "trash", "file", "photos",
        ] {
            assert!(!name.contains(word), "{name}");
        }
    }
}

#[tokio::test]
async fn before_any_refresh_the_root_says_it_was_never_listed() {
    let (_dir, mut server) = bound(Some("yolo"));
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_list", json!({})).await;
    assert_eq!(page["folder"]["id"], ROOT);
    assert_eq!(page["folder"]["path"], "/");
    assert_eq!(page["folder"]["listed"], false);
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["complete"], false);
    assert_eq!(page["source"], "local_cache");
    assert_eq!(page["remote_verified"], false);
    assert_eq!(
        error(&mut server, "drive_list", json!({"path":"/Documents"})).await,
        "path_not_in_cache_run_drive_refresh"
    );
    assert_eq!(
        error(&mut server, "drive_get", json!({"id":DOCS})).await,
        "not_found"
    );
}

#[tokio::test]
async fn refresh_reads_only_drive_listings_and_never_sends_queued_writes() {
    for policy in [None, Some("writes_only"), Some("yolo")] {
        let remote = apple().await;
        serve_tree(&remote).await;
        let (dir, _server) = bound(policy);
        let before = untouched(dir.path());
        let result = refresh(
            dir.path(),
            &remote.uri(),
            json!({"path":format!("/Documents/{DEEP_NAME}")}),
        )
        .await
        .unwrap();
        assert_eq!(result["service"], "drive", "{policy:?}");
        assert_eq!(result["outcome"], "ok");
        assert_eq!(result["folder"]["id"], DEEP);
        assert_eq!(result["folder"]["path"], format!("/Documents/{DEEP_NAME}"));
        assert_eq!(result["folders_read"], 3);
        assert_eq!(result["items_read"], 4);
        assert_eq!(result["queued_drive_changes"], 1);
        assert_eq!(result["queued_changes_sent"], 0);
        assert_eq!(result["file_contents_read"], false);
        assert_eq!(result["remote_verified"], false);
        assert!(!result.to_string().contains("SYNTHETIC-TOKEN"));
        let asked = only_drive_listings(&remote.received_requests().await.unwrap());
        assert_eq!(asked, [ROOT, DOCS, DEEP], "the path, one folder at a time");
        assert_eq!(untouched(dir.path()), before, "{policy:?}");
    }
}

#[tokio::test]
async fn lists_folders_first_with_kinds_and_without_trashed_items() {
    let (_dir, mut server, _remote) = refreshed(Some("yolo")).await;
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_list", json!({"path":"/"})).await;
    assert_eq!(page["folder"]["listed"], true);
    assert_eq!(page["complete"], true);
    assert!(page["folder"]["listed_at"].is_i64());
    assert_eq!(
        names(&page),
        ["Documents", "Pages", "Budget.numbers", "Odd", "report.pdf"]
    );
    let kinds: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["folder", "app_library", "file", "other", "file"]);
    let odd = &page["items"][3];
    assert_eq!(odd["apple_type"], "PACKAGE");
    assert_eq!(odd["is_folder"], false);
    let report = &page["items"][4];
    assert_eq!(report["size"], 20480);
    assert_eq!(report["extension"], "pdf");
    assert_eq!(report["modified_at"], "2026-03-04T05:06:07Z");
    assert_eq!(report["parent_id"], ROOT);
    assert!(!page.to_string().contains("gone"));
    // A package is not a folder.
    assert_eq!(
        error(&mut server, "drive_list", json!({"path":"/Budget.numbers"})).await,
        "not_a_folder"
    );
    assert_eq!(
        error(&mut server, "drive_list", json!({"path":"/Odd"})).await,
        "not_a_folder"
    );
    // The app library was never listed: honest, not empty.
    let pages = ok(&mut server, "drive_list", json!({"id":PAGES})).await;
    assert_eq!(pages["folder"]["listed"], false);
    assert_eq!(pages["complete"], false);
    assert_eq!(pages["folder"]["path"], "/Pages");
}

#[tokio::test]
async fn nested_paths_and_ids_page_in_a_stable_order() {
    let (_dir, mut server, _remote) = refreshed(Some("yolo")).await;
    init(&mut server, false).await;
    let by_path = ok(
        &mut server,
        "drive_list",
        json!({"path":"/Documents","limit":2}),
    )
    .await;
    assert_eq!(names(&by_path), [DEEP_NAME, "Été.txt"]);
    assert_eq!(by_path["total"], 5);
    assert_eq!(by_path["next_offset"], 2);
    let next = ok(
        &mut server,
        "drive_list",
        json!({"id":DOCS,"offset":2,"limit":2}),
    )
    .await;
    assert_eq!(names(&next), ["Scan 9.jpg", "Scan 10.jpg"]);
    let last = ok(
        &mut server,
        "drive_list",
        json!({"id":DOCS,"offset":4,"limit":2}),
    )
    .await;
    assert_eq!(names(&last), ["Scan 11.jpg"]);
    assert_eq!(last["next_offset"], Value::Null);
    let past = ok(&mut server, "drive_list", json!({"id":DOCS,"offset":99})).await;
    assert_eq!(past["items"], json!([]));
    let deep = ok(
        &mut server,
        "drive_list",
        json!({"path":format!("/Documents/{DEEP_NAME}")}),
    )
    .await;
    assert_eq!(deep["folder"]["id"], DEEP);
    assert_eq!(deep["total"], 4);
    let item = ok(
        &mut server,
        "drive_get",
        json!({"path":"/Documents/Scan 9.jpg"}),
    )
    .await;
    assert_eq!(item["item"]["id"], "FILE::com.apple.CloudDocs::S9");
    assert_eq!(item["item"]["path"], "/Documents/Scan 9.jpg");
    assert_eq!(item["item"]["parent_id"], DOCS);
    assert_eq!(item["item"]["size"], 9);
    assert_eq!(item["file_contents_included"], false);
    assert!(item["not_included"].as_array().unwrap().len() >= 3);
    let folder = ok(&mut server, "drive_get", json!({"id":DOCS})).await;
    assert_eq!(folder["item"]["kind"], "folder");
    assert_eq!(folder["item"]["child_count"], 2);
    assert_eq!(folder["item"]["created_at"], "2026-01-02T03:04:05Z");
    assert_eq!(folder["listed"], true);
    // Exact names only: a different case is a different path.
    assert_eq!(
        error(&mut server, "drive_get", json!({"path":"/documents"})).await,
        "not_found"
    );
}

#[tokio::test]
async fn odd_names_are_data_and_long_text_is_cut_but_ids_never_are() {
    let (_dir, mut server, _remote) = refreshed(Some("yolo")).await;
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_list", json!({"id":DEEP})).await;
    let items = page["items"].as_array().unwrap();
    let find = |id: &str| items.iter().find(|i| i["id"] == id).unwrap().clone();
    let bell = find("FILE::com.apple.CloudDocs::BELL");
    assert_eq!(bell["display_name"], "bell\u{7}name.txt");
    let long = find("FILE::com.apple.CloudDocs::LONG");
    assert_eq!(long["name_truncated"], true);
    assert_eq!(long["name"].as_str().unwrap().chars().count(), 512);
    assert_eq!(page["names_truncated"], 1);
    let unsupported: Vec<&Value> = items.iter().filter(|i| i["id"].is_null()).collect();
    assert_eq!(unsupported.len(), 1);
    assert_eq!(unsupported[0]["unsupported"], "id_too_long");
    assert_eq!(page["unsupported_items"], 1);
    // A name with a control character or a slash cannot be named by path; its id still works.
    let got = ok(
        &mut server,
        "drive_get",
        json!({"id":"FILE::com.apple.CloudDocs::BELL"}),
    )
    .await;
    assert_eq!(got["item"]["path"], Value::Null);
    assert_eq!(got["path_addressable"], false);
    let got = ok(
        &mut server,
        "drive_get",
        json!({"id":"FILE::com.apple.CloudDocs::SLASH"}),
    )
    .await;
    assert_eq!(got["item"]["path"], Value::Null);
    let got = ok(
        &mut server,
        "drive_get",
        json!({"id":"FILE::com.apple.CloudDocs::LONG"}),
    )
    .await;
    assert_eq!(got["item"]["name"].as_str().unwrap().chars().count(), 3000);
    assert_eq!(got["item_truncated"], false);
}

#[tokio::test]
async fn paths_and_ids_are_validated_before_any_keyring_or_network_use() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    assert_eq!(
        error(&mut server, "drive_refresh", json!({})).await,
        "login_required"
    );
    let deep = format!("/{}", vec!["a"; 33].join("/"));
    let long = format!("/{}", "a".repeat(256));
    let longer = format!("/{}", vec!["a".repeat(200); 6].join("/"));
    for tool in ["drive_list", "drive_get", "drive_refresh"] {
        for (args, code) in [
            (json!({"path":""}), "invalid_path"),
            (json!({"path":"Documents"}), "invalid_path"),
            (json!({"path":"/Documents/"}), "invalid_path"),
            (json!({"path":"/a//b"}), "invalid_path"),
            (json!({"path":"/a/../b"}), "invalid_path"),
            (json!({"path":"/.."}), "invalid_path"),
            (json!({"path":"/a/./b"}), "invalid_path"),
            (json!({"path":"/a\u{0}b"}), "invalid_path"),
            (json!({"path":"/a\u{1b}b"}), "invalid_path"),
            (json!({"path":"/a\nb"}), "invalid_path"),
            (json!({"path":long}), "invalid_path"),
            (json!({"path":deep}), "path_too_deep"),
            (json!({"path":longer}), "path_too_long"),
            (json!({"path":"/a","id":ROOT}), "invalid_arguments"),
            (json!({"id":""}), "invalid_id"),
            (json!({"id":"x".repeat(513)}), "invalid_id"),
            (json!({"id":"FOLDER::a\u{0}"}), "invalid_id"),
            (json!({"path":"/a","full":true}), "invalid_arguments"),
            (json!({"path":7}), "invalid_arguments"),
        ] {
            assert_eq!(
                error(&mut server, tool, args.clone()).await,
                code,
                "{tool} {args}"
            );
        }
    }
    for tool in ["drive_list", "drive_refresh"] {
        assert_eq!(
            error(&mut server, tool, json!({"id":"TRASH_ROOT"})).await,
            "trash_not_supported"
        );
        assert_eq!(
            error(
                &mut server,
                tool,
                json!({"id":"FILE::com.apple.CloudDocs::REPORT"})
            )
            .await,
            "not_a_folder"
        );
    }
    assert_eq!(
        error(&mut server, "drive_get", json!({})).await,
        "invalid_arguments"
    );
    for (args, code) in [
        (json!({"limit":0}), "invalid_arguments"),
        (json!({"limit":101}), "invalid_arguments"),
        (json!({"offset":-1}), "invalid_arguments"),
    ] {
        assert_eq!(error(&mut server, "drive_list", args).await, code);
    }
    for (args, code) in [
        (json!({"query":""}), "query_required"),
        (json!({"query":"  "}), "query_required"),
        (json!({"query":"x".repeat(257)}), "query_too_long"),
        (json!({"query":"x","limit":101}), "invalid_arguments"),
        (json!({}), "invalid_arguments"),
    ] {
        assert_eq!(error(&mut server, "drive_search", args).await, code);
    }
}

#[tokio::test]
async fn search_finds_names_across_listed_folders_without_trash() {
    let (_dir, mut server, _remote) = refreshed(Some("yolo")).await;
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_search", json!({"query":"scan"})).await;
    let mut found = names(&page);
    found.sort();
    assert_eq!(found, ["Scan 10.jpg", "Scan 11.jpg", "Scan 9.jpg"]);
    assert_eq!(page["total"], 3);
    assert_eq!(page["matches_capped"], false);
    assert!(page["items"][0]["path"]
        .as_str()
        .unwrap()
        .starts_with("/Documents/Scan"));
    // Case and accents aside.
    let page = ok(&mut server, "drive_search", json!({"query":"ete"})).await;
    assert_eq!(names(&page), ["Été.txt"]);
    let page = ok(&mut server, "drive_search", json!({"query":"unicode"})).await;
    assert_eq!(ids(&page), [DEEP]);
    // Recently Deleted is never cached or found.
    let page = ok(&mut server, "drive_search", json!({"query":"gone"})).await;
    assert_eq!(page["items"], json!([]));
    let first = ok(
        &mut server,
        "drive_search",
        json!({"query":"scan","limit":2}),
    )
    .await;
    assert_eq!(first["next_offset"], 2);
    let rest = ok(
        &mut server,
        "drive_search",
        json!({"query":"scan","offset":2}),
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_folder_gone_from_its_parent_takes_its_cached_subtree_with_it() {
    let (dir, mut server, remote) = refreshed(Some("yolo")).await;
    remote.reset().await;
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200).set_body_json(listing(
            ROOT,
            json!([file(
                "FILE::com.apple.CloudDocs::REPORT",
                "report",
                "pdf",
                1
            )]),
        )),
    )
    .await;
    refresh(dir.path(), &remote.uri(), json!({})).await.unwrap();
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_list", json!({})).await;
    assert_eq!(names(&page), ["report.pdf"]);
    assert_eq!(
        error(&mut server, "drive_list", json!({"id":DEEP})).await,
        "not_found"
    );
    assert_eq!(
        error(
            &mut server,
            "drive_get",
            json!({"id":"FILE::com.apple.CloudDocs::S9"})
        )
        .await,
        "not_found"
    );
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert!(library.drive_children(DOCS).unwrap().is_empty());
    assert!(library.drive_children(DEEP).unwrap().is_empty());
    let listed = library.state("mcp.drive.listed").unwrap().unwrap();
    assert!(
        !listed.contains("DOCS") && !listed.contains("DEEP"),
        "{listed}"
    );
    // A refresh by id reads that folder alone.
    let remote = apple().await;
    serve_tree(&remote).await;
    refresh(dir.path(), &remote.uri(), json!({"id":ROOT}))
        .await
        .unwrap();
    assert_eq!(
        only_drive_listings(&remote.received_requests().await.unwrap()),
        [ROOT]
    );
}

#[tokio::test]
async fn a_failure_part_way_down_a_path_stores_nothing_and_keeps_apple_text_out() {
    let (dir, mut server, _old) = refreshed(Some("yolo")).await;
    let kept = cached(dir.path());
    let before = untouched(dir.path());
    // The root reads with a change, then Documents fails: neither is stored.
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200).set_body_json(listing(ROOT, json!([folder(DOCS, "Documents")]))),
    )
    .await;
    serve(
        &remote,
        DOCS,
        ResponseTemplate::new(503).set_body_string("SYNTHETIC-UPSTREAM-BODY"),
    )
    .await;
    assert_eq!(
        refresh(
            dir.path(),
            &remote.uri(),
            json!({"path":format!("/Documents/{DEEP_NAME}")})
        )
        .await
        .unwrap_err(),
        "drive_refresh_failed_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert_eq!(untouched(dir.path()), before);
    assert!(drive_failed(dir.path()));
    init(&mut server, false).await;
    let page = ok(&mut server, "drive_list", json!({})).await;
    assert_eq!(page["cache"]["last_refresh_failed"], true);
    assert_eq!(page["items"].as_array().unwrap().len(), 5);
    assert!(!page.to_string().contains("SYNTHETIC-UPSTREAM-BODY"));
    let health = quince_core::Library::open(dir.path())
        .unwrap()
        .sync_health()
        .unwrap();
    assert!(!format!("{health:?}").contains("SYNTHETIC-UPSTREAM-BODY"));
}

#[tokio::test]
async fn odd_answers_are_refused_and_the_cache_kept() {
    let cases: Vec<(&str, ResponseTemplate, &str)> = vec![
        (
            "gone",
            ResponseTemplate::new(200)
                .set_body_json(json!([{"drivewsid":ROOT,"status":"ID_INVALID","items":[]}])),
            "drive_folder_not_found_cache_kept",
        ),
        (
            "status",
            ResponseTemplate::new(200)
                .set_body_json(json!([{"drivewsid":ROOT,"status":"UNKNOWN","items":[]}])),
            "drive_refresh_failed_cache_kept",
        ),
        (
            "no items",
            ResponseTemplate::new(200).set_body_json(json!([{"drivewsid":ROOT,"status":"OK"}])),
            "drive_refresh_failed_cache_kept",
        ),
        (
            "not a list",
            ResponseTemplate::new(200).set_body_json(json!({"items":[]})),
            "drive_refresh_failed_cache_kept",
        ),
        (
            "not json",
            ResponseTemplate::new(200).set_body_string("<html>"),
            "drive_refresh_failed_cache_kept",
        ),
        (
            "refused",
            ResponseTemplate::new(400).set_body_string("SYNTHETIC-UPSTREAM-BODY"),
            "drive_refresh_failed_cache_kept",
        ),
        (
            "rate limited",
            ResponseTemplate::new(423).set_body_string("SYNTHETIC-UPSTREAM-BODY"),
            "drive_refresh_failed_cache_kept",
        ),
        ("reauth", ResponseTemplate::new(450), "login_required"),
    ];
    for (what, answer, code) in cases {
        let (dir, _server, _old) = refreshed(Some("yolo")).await;
        let kept = cached(dir.path());
        let remote = apple().await;
        serve(&remote, ROOT, answer).await;
        assert_eq!(
            refresh(dir.path(), &remote.uri(), json!({}))
                .await
                .unwrap_err(),
            code,
            "{what}"
        );
        assert_eq!(cached(dir.path()), kept, "{what}");
    }
}

#[tokio::test]
async fn oversized_answers_and_too_many_items_are_refused_and_the_cache_kept() {
    let small = || skykeep_mcp::drive::RefreshLimits {
        max_response_bytes: 4096,
        max_total_bytes: 16_384,
        max_items: 4,
        max_item_bytes: 1024,
        ..Default::default()
    };
    let many: Vec<Value> = (0..5)
        .map(|i| file(&format!("FILE::z::{i}"), "f", "txt", 1))
        .collect();
    let cases = [
        (
            listing(
                ROOT,
                json!([file("FILE::z::big", &"x".repeat(5000), "txt", 1)]),
            ),
            "drive_response_too_large_cache_kept",
        ),
        (
            listing(ROOT, json!(many)),
            "drive_folder_too_large_cache_kept",
        ),
        (
            listing(
                ROOT,
                json!([file("FILE::z::big", &"x".repeat(1500), "txt", 1)]),
            ),
            "drive_item_too_large_cache_kept",
        ),
    ];
    for (body, code) in cases {
        let (dir, _server, _old) = refreshed(Some("yolo")).await;
        let kept = cached(dir.path());
        let remote = apple().await;
        serve(
            &remote,
            ROOT,
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;
        assert_eq!(
            refresh_limited(dir.path(), &remote.uri(), json!({}), small())
                .await
                .unwrap_err(),
            code
        );
        assert_eq!(cached(dir.path()), kept, "{code}");
        assert!(drive_failed(dir.path()), "{code}");
    }
    // The total budget across the folders of a path, counted as the bodies arrive.
    let (dir, _server, _old) = refreshed(Some("yolo")).await;
    let kept = cached(dir.path());
    let remote = apple().await;
    let padded = |id: &str, child: Value| {
        listing(
            id,
            json!([child, file("FILE::z::pad", &"p".repeat(700), "txt", 1)]),
        )
    };
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200).set_body_json(padded(ROOT, folder(DOCS, "Documents"))),
    )
    .await;
    serve(
        &remote,
        DOCS,
        ResponseTemplate::new(200).set_body_json(padded(DOCS, folder(DEEP, DEEP_NAME))),
    )
    .await;
    serve(
        &remote,
        DEEP,
        ResponseTemplate::new(200).set_body_json(padded(DEEP, file("FILE::z::x", "x", "txt", 1))),
    )
    .await;
    let tight = skykeep_mcp::drive::RefreshLimits {
        max_total_bytes: 2000,
        ..Default::default()
    };
    assert_eq!(
        refresh_limited(
            dir.path(),
            &remote.uri(),
            json!({"path":format!("/Documents/{DEEP_NAME}")}),
            tight
        )
        .await
        .unwrap_err(),
        "drive_response_too_large_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn a_path_apple_does_not_have_stores_nothing() {
    let (dir, _server, _old) = refreshed(Some("yolo")).await;
    let kept = cached(dir.path());
    let remote = apple().await;
    serve_tree(&remote).await;
    for (path, code) in [
        ("/Nope", "drive_path_not_found_cache_kept"),
        ("/report.pdf", "drive_not_a_folder_cache_kept"),
        ("/Budget.numbers/inside", "drive_not_a_folder_cache_kept"),
    ] {
        assert_eq!(
            refresh(dir.path(), &remote.uri(), json!({"path":path}))
                .await
                .unwrap_err(),
            code,
            "{path}"
        );
        assert_eq!(cached(dir.path()), kept, "{path}");
    }
    // Two items Apple lists under one name: refused rather than guessed.
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200).set_body_json(listing(
            ROOT,
            json!([folder(DOCS, "Twin"), folder(DEEP, "Twin")]),
        )),
    )
    .await;
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({"path":"/Twin"}))
            .await
            .unwrap_err(),
        "drive_ambiguous_path_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn advanced_data_protection_refuses_without_storing_or_asking_for_approval() {
    // Apple's own refusal for want of the key.
    let (dir, _server, _old) = refreshed(Some("yolo")).await;
    let kept = cached(dir.path());
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(423).set_body_string(
            r#"{"errorReason":"Missing PCS cookies from the request","errorCode":423}"#,
        ),
    )
    .await;
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "drive_paused_approve_on_apple_device"
    );
    assert_eq!(cached(dir.path()), kept);
    for r in remote.received_requests().await.unwrap() {
        let p = r.url.path().to_string();
        assert!(
            p == "/setup/ws/1/validate" || p == "/retrieveItemDetailsInFolders",
            "no approval request: {p}"
        );
    }
    // A session known to lack the key never asks Drive.
    let remote = apple().await;
    serve_tree(&remote).await;
    let result = skykeep_mcp::drive::refresh_with_client(
        dir.path(),
        saved_client_with(dir.path(), &remote.uri(), true),
        json!({}),
    )
    .await;
    private_files(dir.path());
    assert_eq!(result.unwrap_err(), "drive_paused_approve_on_apple_device");
    assert!(only_drive_listings(&remote.received_requests().await.unwrap()).is_empty());
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn refresh_fails_closed_on_session_account_or_service_problems() {
    // A session for another account reads nothing.
    let remote = apple().await;
    serve_tree(&remote).await;
    let dir = private_dir();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "account_mismatch_use_new_state_directory"
    );
    assert!(only_drive_listings(&remote.received_requests().await.unwrap()).is_empty());
    assert!(quince_core::Library::open(dir.path())
        .unwrap()
        .drive_children(ROOT)
        .unwrap()
        .is_empty());
    // No Drive service in the session.
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"ckdatabasews":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let (dir, _server) = bound(Some("yolo"));
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
        skykeep_mcp::drive::refresh_with_client(dir.path(), client, json!({}))
            .await
            .unwrap_err(),
        "drive_service_unavailable"
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
    // An id this cache does not hold is refused before anything is asked.
    let remote = apple().await;
    let (dir, _server) = bound(Some("yolo"));
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({"id":DOCS}))
            .await
            .unwrap_err(),
        "folder_not_in_cache_use_path"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_and_writes_refresh_without_approval_touches_nothing() {
    let remote = apple().await;
    let (dir, _server) = bound(Some("reads_and_writes"));
    let before = untouched(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri(), json!({}))
            .await
            .unwrap_err(),
        "read_approval_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
    assert_eq!(untouched(dir.path()), before);
}

#[tokio::test]
async fn reads_and_writes_binds_each_drive_read_to_one_approval() {
    let (_dir, mut server, _remote) = refreshed(Some("reads_and_writes")).await;
    init(&mut server, true).await;
    let form = call(&mut server, 3, "drive_list", json!({"path":"/Documents"})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("drive\\u005flist"), "{message}");
    assert!(!message.contains("Scan"), "no content before approval");
    assert_eq!(
        text(&call(&mut server, 4, "drive_search", json!({"query":"scan"})).await)["error"],
        "approval_pending"
    );
    let accepted = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(text(&accepted)["total"], 5);
    // One approval, one call.
    let form = call(&mut server, 5, "drive_get", json!({"id":DOCS})).await;
    assert_eq!(form["method"], "elicitation/create");
    let denied = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&denied)["error"], "approval_denied");
    // Malformed arguments are refused before anyone is asked.
    assert_eq!(
        text(&call(&mut server, 6, "drive_list", json!({"path":"/a/../b"})).await)["error"],
        "invalid_path"
    );
    // The refresh prompt names Drive alone and says nothing is downloaded or sent.
    let form = call(
        &mut server,
        8,
        "drive_refresh",
        json!({"path":"/Documents"}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("drive\\u005frefresh"), "{message}");
    assert!(
        message.contains("\"services\": \\u005b\n      \"drive\"\n    \\u005d"),
        "{message}"
    );
    assert!(message.contains("/Documents"), "{message}");
    assert!(message.contains("nothing is sent"), "{message}");
    assert!(message.contains("no file contents"), "{message}");
    for other in ["notes", "reminders", "contacts", "calendar"] {
        assert!(!message.contains(other), "{message}");
    }
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_denied");
    // Without form support, nothing is read.
    let (_dir, mut plain, _remote) = refreshed(Some("reads_and_writes")).await;
    init(&mut plain, false).await;
    assert_eq!(
        error(&mut plain, "drive_list", json!({})).await,
        "approval_requires_form_client"
    );
}

#[tokio::test]
async fn writes_only_and_yolo_read_drive_without_a_prompt() {
    for mode in ["writes_only", "yolo"] {
        let (_dir, mut server, _remote) = refreshed(Some(mode)).await;
        init(&mut server, true).await;
        for (name, args) in [
            ("drive_list", json!({})),
            ("drive_get", json!({"id":DOCS})),
            ("drive_search", json!({"query":"scan"})),
        ] {
            let reply = call(&mut server, 3, name, args).await;
            assert!(reply.get("method").is_none(), "{mode} {name}: {reply}");
            assert_eq!(reply["result"]["isError"], false, "{mode} {name}: {reply}");
        }
    }
}

#[tokio::test]
async fn a_slow_answer_times_out_stores_nothing_and_is_recorded() {
    let (dir, _server, _old) = refreshed(Some("yolo")).await;
    let kept = cached(dir.path());
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200)
            .set_body_json(listing(ROOT, root_items()))
            .set_delay(std::time::Duration::from_secs(5)),
    )
    .await;
    let limits = skykeep_mcp::drive::RefreshLimits {
        timeout: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), json!({}), limits)
            .await
            .unwrap_err(),
        "drive_refresh_timeout_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(drive_failed(dir.path()));
}

#[tokio::test]
async fn an_account_change_during_refresh_stores_nothing() {
    let remote = apple().await;
    serve(
        &remote,
        ROOT,
        ResponseTemplate::new(200)
            .set_body_json(listing(ROOT, root_items()))
            .set_delay(std::time::Duration::from_millis(300)),
    )
    .await;
    let (dir, _server) = bound(Some("yolo"));
    let path_buf = dir.path().to_owned();
    let uri = remote.uri();
    let running = tokio::spawn(async move { refresh(&path_buf, &uri, json!({})).await });
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
    assert!(library.drive_children(ROOT).unwrap().is_empty());
    assert!(!drive_failed(dir.path()));
}

#[tokio::test]
async fn pages_fit_the_cap_with_many_huge_names() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    let items: Vec<quince_core::drive::DriveItem> = (0..100)
        .map(|i| quince_core::drive::DriveItem {
            drivewsid: format!("FILE::z::{i:03}{}", "i".repeat(400)),
            parent_id: None,
            name: format!("{i:03}{}", "\u{7}\"<".repeat(20_000)),
            extension: Some("e".repeat(10_000)),
            kind: "FILE".into(),
            size: Some(1),
            child_count: None,
            created_at: None,
            modified_at: None,
            etag: None,
        })
        .collect();
    library.put_drive_children(ROOT, &items).unwrap();
    library
        .set_state("mcp.drive.listed", &json!({ROOT: 1}).to_string())
        .unwrap();
    init(&mut server, false).await;
    let reply = call(&mut server, 3, "drive_list", json!({"limit":100})).await;
    let frame = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(serde_json::to_string(frame).unwrap().len() <= 262_144);
    let page = text(&reply);
    let shown = page["items"].as_array().unwrap().len();
    assert!(shown >= 1);
    for item in page["items"].as_array().unwrap() {
        assert!(item["id"].as_str().unwrap().len() > 400, "ids are whole");
    }
    if shown < 100 {
        assert_eq!(page["page_shortened_to_fit"], true);
        assert_eq!(page["next_offset"], shown);
    }
    let got = ok(&mut server, "drive_get", json!({"id":items[0].drivewsid})).await;
    assert_eq!(got["item_truncated"], true);
    assert_eq!(got["item"]["id"], items[0].drivewsid);
}
