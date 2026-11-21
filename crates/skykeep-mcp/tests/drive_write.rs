//! Apple iCloud Drive changes through `drive_write`: each action against a loopback mock of
//! Apple, the approval each needs, and the exchange folder's limits. Every fixture is
//! synthetic; nothing reaches the network.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::drive_write::{Seam, WriteLimits};
use skykeep_mcp::Server;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{body_json, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DSID: &str = "1234567890";
const ROOT: &str = "FOLDER::com.apple.CloudDocs::root";
const DOCS: &str = "FOLDER::com.apple.CloudDocs::DOCS";
const BIG: &str = "FOLDER::com.apple.CloudDocs::BIG";
const SMALL: &str = "FOLDER::com.apple.CloudDocs::SMALL";
const REPORT: &str = "FILE::com.apple.CloudDocs::REPORT";
const NOTES: &str = "FILE::com.apple.CloudDocs::NOTES";

fn private_dir() -> tempfile::TempDir {
    // Files the synthetic session store writes are private, as in production.
    unsafe {
        libc::umask(0o077);
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn folder(id: &str, name: &str, count: i64) -> Value {
    json!({"drivewsid":id,"parentId":ROOT,"name":name,"type":"FOLDER","directChildrenCount":count,"etag":"f1"})
}

fn file(id: &str, parent: &str, name: &str, ext: &str, size: i64) -> Value {
    json!({"drivewsid":id,"parentId":parent,"name":name,"extension":ext,"type":"FILE","size":size,"etag":"e1"})
}

fn listing(id: &str, items: Value) -> Value {
    json!([{"drivewsid":id,"status":"OK","items":items}])
}

fn root_items() -> Value {
    json!([
        folder(DOCS, "Documents", 2),
        folder(BIG, "Big", 500),
        folder(SMALL, "Small", 1),
        file(REPORT, ROOT, "report", "pdf", 11),
    ])
}

fn docs_items() -> Value {
    json!([file(NOTES, DOCS, "notes", "txt", 5)])
}

/// The answer to a folder read, for as many reads as `times` (all of them when `None`).
async fn serve_listing(remote: &MockServer, id: &str, items: Value, times: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path("/retrieveItemDetailsInFolders"))
        .and(body_json(json!([{"drivewsid":id,"partialData":false}])))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing(id, items)));
    match times {
        Some(n) => mock.up_to_n_times(n).with_priority(1).mount(remote).await,
        None => mock.with_priority(5).mount(remote).await,
    }
}

async fn apple() -> MockServer {
    let remote = MockServer::start().await;
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"drivews":{"url":uri},"docws":{"url":uri}}
        })))
        .mount(&remote)
        .await;
    remote
}

fn client(dir: &Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"drivews":{"url":uri},"docws":{"url":uri}})),
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

fn quick() -> WriteLimits {
    WriteLimits {
        change_timeout: Duration::from_secs(20),
        transfer_timeout: Duration::from_secs(20),
        ..WriteLimits::default()
    }
}

/// A bound directory whose cache holds the root and Documents as read, an exchange folder,
/// and the server pointed at `remote`.
struct Fixture {
    dir: tempfile::TempDir,
    exchange: PathBuf,
    server: Server,
    remote: MockServer,
}

async fn fixture(policy: Option<&str>) -> Fixture {
    let remote = apple().await;
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
    }
    // The cache as drive_refresh leaves it: the root and Documents read.
    serve_listing(&remote, ROOT, root_items(), Some(1)).await;
    serve_listing(&remote, DOCS, docs_items(), Some(1)).await;
    skykeep_mcp::drive::refresh_with_client(
        dir.path(),
        client(dir.path(), &remote.uri()),
        json!({"path":"/Documents"}),
    )
    .await
    .unwrap();
    remote.reset().await;
    let remote = {
        // `reset` drops the sign-in answer too; mount it again.
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/validate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "dsInfo":{"dsid":DSID},
                "webservices":{"drivews":{"url":remote.uri()},"docws":{"url":remote.uri()}}
            })))
            .mount(&remote)
            .await;
        remote
    };
    let exchange = dir.path().parent().unwrap().join(format!(
        "exchange-{}",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir(&exchange).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        skykeep_mcp::exchange::set_up(&mut library, Some(exchange.clone()), None).unwrap();
    }
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    server.set_drive_seam(seam(&remote.uri(), quick()));
    init(&mut server, true).await;
    Fixture {
        dir,
        exchange,
        server,
        remote,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.exchange);
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
        .handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"drive_write","arguments":args}}))
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

/// The requests that ask Apple to change something, by path.
async fn writes(remote: &MockServer) -> Vec<String> {
    remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| {
            ![
                "/setup/ws/1/validate",
                "/retrieveItemDetailsInFolders",
                "/retrieveItemDetails",
            ]
            .contains(&p.as_str())
                && !p.contains("/download/")
                && !p.starts_with("/bytes")
        })
        .collect()
}

async fn requests(remote: &MockServer) -> Vec<Request> {
    remote.received_requests().await.unwrap()
}

fn ok_items(id: &str) -> Value {
    json!({"items":[{"drivewsid":id,"status":"OK"}]})
}

async fn answer(remote: &MockServer, at: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(remote)
        .await;
}

fn proposal_id(value: &Value) -> String {
    assert_eq!(value["status"], "approval_required", "{value}");
    value["request_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_tool_schema_is_strict_and_names_each_action() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "drive_write")
        .unwrap()
        .clone();
    let schema = &tool["inputSchema"];
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["action"]));
    assert_eq!(
        schema["properties"]["action"]["enum"],
        json!([
            "create_folder",
            "rename",
            "move",
            "trash",
            "delete_forever",
            "upload",
            "download"
        ])
    );
    let mut keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "action",
            "destination_id",
            "destination_path",
            "id",
            "local",
            "name",
            "path"
        ]
    );
    assert_eq!(tool["annotations"]["destructiveHint"], true);
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    assert!(tool["description"]
        .as_str()
        .unwrap()
        .contains("Apple iCloud Drive"));
    for action in skykeep_mcp::drive_write::ACTIONS {
        assert_eq!(
            skykeep_mcp::drive_write::is_destructive(action),
            ["trash", "delete_forever"].contains(&action),
            "{action}"
        );
    }
}

#[tokio::test]
async fn create_folder_is_done_and_a_taken_name_is_a_collision() {
    let mut f = fixture(None).await;
    serve_listing(&f.remote, DOCS, docs_items(), None).await;
    answer(
        &f.remote,
        "/createFolders",
        json!({"folders":[{"drivewsid":"FOLDER::com.apple.CloudDocs::NEW","name":"Taxes","type":"FOLDER","status":"OK"}]}),
    )
    .await;
    let done = ok(
        &mut f.server,
        json!({"action":"create_folder","path":"/Documents","name":"Taxes"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["folder"]["id"], "FOLDER::com.apple.CloudDocs::NEW");
    assert_eq!(done["remote_verified"], false);
    assert_eq!(writes(&f.remote).await, ["/createFolders"]);
    let sent: Value = serde_json::from_slice(
        &requests(&f.remote)
            .await
            .into_iter()
            .find(|r| r.url.path() == "/createFolders")
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(sent["destinationDrivewsId"], DOCS);
    // Documents is marked for a refresh: the cache no longer says what is in it.
    let library = quince_core::Library::open(f.dir.path()).unwrap();
    assert!(!library
        .state("mcp.drive.listed")
        .unwrap()
        .unwrap()
        .contains(DOCS));
    drop(library);
    // A second try finds the name at Apple and adds nothing.
    f.remote.reset().await;
    let f2 = &mut f;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"dsInfo":{"dsid":DSID},"webservices":{"drivews":{"url":f2.remote.uri()}}}),
        ))
        .mount(&f2.remote)
        .await;
    serve_listing(&f2.remote, ROOT, root_items(), None).await;
    assert_eq!(
        error(
            &mut f2.server,
            json!({"action":"create_folder","path":"/","name":"REPORT.pdf"})
        )
        .await,
        "name_taken"
    );
    assert!(writes(&f2.remote).await.is_empty());
}

#[tokio::test]
async fn a_missing_parent_is_refused_before_anything_is_sent() {
    let mut f = fixture(None).await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"create_folder","id":"FOLDER::com.apple.CloudDocs::NONE","name":"x"})
        )
        .await,
        "parent_not_found"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"create_folder","path":"/Nowhere","name":"x"})
        )
        .await,
        "parent_not_found"
    );
    // Cached, but Apple no longer has it.
    Mock::given(method("POST"))
        .and(path("/retrieveItemDetailsInFolders"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"drivewsid":DOCS,"status":"ID_INVALID","items":[]}])),
        )
        .mount(&f.remote)
        .await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"a.txt"})
        )
        .await,
        "local_file_not_found"
    );
    std::fs::write(f.exchange.join("a.txt"), b"abc").unwrap();
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"create_folder","path":"/Documents","name":"x"})
        )
        .await,
        "parent_not_found"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"a.txt"})
        )
        .await,
        "parent_not_found"
    );
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn rename_is_done_and_collisions_and_repeats_are_said() {
    let mut f = fixture(None).await;
    let renamed = json!([
        file(REPORT, ROOT, "final", "pdf", 11),
        folder(DOCS, "Documents", 2)
    ]);
    // Before the change: as cached. After: Apple shows the new name.
    serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
    serve_listing(&f.remote, ROOT, renamed, None).await;
    answer(&f.remote, "/renameItems", ok_items(REPORT)).await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","path":"/report.pdf","name":"documents"})
        )
        .await,
        "name_taken",
        "names compare without case"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","path":"/report.pdf","name":"report.pdf"})
        )
        .await,
        "already_has_this_name"
    );
    assert!(writes(&f.remote).await.is_empty());
    let done = ok(
        &mut f.server,
        json!({"action":"rename","path":"/report.pdf","name":"final.pdf"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], true);
    assert_eq!(writes(&f.remote).await, ["/renameItems"]);
}

#[tokio::test]
async fn an_item_changed_at_apple_since_it_was_cached_is_not_changed() {
    let mut f = fixture(None).await;
    let mut items = root_items();
    items[3]["etag"] = json!("e2");
    serve_listing(&f.remote, ROOT, items, None).await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","id":REPORT,"name":"final.pdf"})
        )
        .await,
        "drive_item_changed_run_drive_refresh"
    );
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn apple_refusing_a_change_is_an_error_without_apple_text() {
    let mut f = fixture(None).await;
    serve_listing(&f.remote, ROOT, root_items(), None).await;
    Mock::given(method("POST"))
        .and(path("/renameItems"))
        .respond_with(ResponseTemplate::new(400).set_body_string("SECRET-APPLE-TEXT"))
        .mount(&f.remote)
        .await;
    let reply = call(
        &mut f.server,
        json!({"action":"rename","id":REPORT,"name":"final.pdf"}),
    )
    .await;
    assert_eq!(reply["result"]["isError"], true);
    let shown = reply.to_string();
    assert!(
        shown.contains("drive_write_not_confirmed_run_drive_refresh"),
        "{shown}"
    );
    assert!(!shown.contains("SECRET"), "{shown}");
    assert!(!shown.contains("SYNTHETIC-TOKEN"), "{shown}");
}

#[tokio::test]
async fn move_of_a_small_folder_runs_and_a_large_one_waits_with_a_preview() {
    let mut f = fixture(None).await;
    // Small: into Documents.
    serve_listing(&f.remote, DOCS, docs_items(), Some(1)).await;
    serve_listing(
        &f.remote,
        DOCS,
        json!([
            file(NOTES, DOCS, "notes", "txt", 5),
            folder(SMALL, "Small", 1)
        ]),
        None,
    )
    .await;
    serve_listing(&f.remote, ROOT, root_items(), None).await;
    answer(&f.remote, "/moveItems", ok_items(SMALL)).await;
    let done = ok(
        &mut f.server,
        json!({"action":"move","path":"/Small","destination_path":"/Documents"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(writes(&f.remote).await, ["/moveItems"]);
    // Large: nothing is sent until the person approves the preview.
    let waiting = ok(
        &mut f.server,
        json!({"action":"move","id":BIG,"destination_id":DOCS}),
    )
    .await;
    let id = proposal_id(&waiting);
    assert_eq!(writes(&f.remote).await, ["/moveItems"]);
    let details = f.server.approval_details(&id).unwrap();
    let details: Value = serde_json::from_str(&details).unwrap();
    assert_eq!(
        details["preview"]["affects"]["apple_item_count"], 500,
        "{details}"
    );
    assert_eq!(details["preview"]["destination"]["id"], DOCS);
    assert!(!details.to_string().contains("SYNTHETIC-TOKEN"));
    // Renaming it waits the same way.
    let waiting = ok(
        &mut f.server,
        json!({"action":"rename","id":BIG,"name":"Huge"}),
    )
    .await;
    proposal_id(&waiting);
    // Moving into itself is refused.
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"move","id":DOCS,"destination_id":DOCS})
        )
        .await,
        "cannot_move_a_folder_into_itself"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"move","id":NOTES,"destination_id":DOCS})
        )
        .await,
        "already_in_destination"
    );
}

/// Trash and delete forever wait for `skykeep-mcp approve`; approved, trash moves the item
/// to Recently Deleted only, and delete forever then deletes it from there.
#[tokio::test]
async fn trash_and_delete_forever_wait_for_the_terminal_and_differ() {
    for (action, deleted) in [("trash", false), ("delete_forever", true)] {
        let mut f = fixture(None).await;
        let id = proposal_id(&ok(&mut f.server, json!({"action":action,"id":REPORT})).await);
        assert!(writes(&f.remote).await.is_empty(), "{action}");
        // Two reads see it; afterwards it is gone from the top folder.
        serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
        serve_listing(&f.remote, ROOT, json!([folder(DOCS, "Documents", 2)]), None).await;
        answer(&f.remote, "/moveItemsToTrash", ok_items(REPORT)).await;
        serve_listing(
            &f.remote,
            "TRASH_ROOT",
            json!([file(REPORT, "TRASH_ROOT", "report", "pdf", 11)]),
            Some(1),
        )
        .await;
        serve_listing(&f.remote, "TRASH_ROOT", json!([]), None).await;
        answer(&f.remote, "/deleteItems", ok_items(REPORT)).await;
        let done = f.server.approve_any(&id).await.unwrap();
        assert_eq!(done["outcome"], "done", "{action}: {done}");
        let mut expected = vec!["/moveItemsToTrash"];
        if deleted {
            expected.push("/deleteItems");
        }
        assert_eq!(writes(&f.remote).await, expected, "{action}");
        // Used up: it never runs twice.
        assert!(f.server.approve_any(&id).await.is_err());
        let library = quince_core::Library::open(f.dir.path()).unwrap();
        assert!(library.drive_item(REPORT).unwrap().is_none(), "{action}");
    }
}

#[tokio::test]
async fn deletions_need_approval_under_every_mode() {
    // yolo and no mode: the terminal.
    for mode in [None, Some("yolo")] {
        let mut f = fixture(mode).await;
        for action in ["trash", "delete_forever"] {
            let value = ok(&mut f.server, json!({"action":action,"id":REPORT})).await;
            proposal_id(&value);
            assert!(value["approve_in_terminal"]
                .as_str()
                .unwrap()
                .contains("skykeep-mcp approve"));
        }
        assert!(writes(&f.remote).await.is_empty());
    }
    // The other modes: the approval form, with the preview.
    for mode in ["writes_only", "reads_and_writes"] {
        let mut f = fixture(Some(mode)).await;
        let form = call(&mut f.server, json!({"action":"trash","id":DOCS})).await;
        assert_eq!(form["method"], "elicitation/create", "{mode}: {form}");
        let mut message = form["params"]["message"].as_str().unwrap().to_string();
        if mode == "reads_and_writes" {
            // First the preview read, then the change.
            let next = f
                .server
                .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
                .await
                .unwrap();
            assert_eq!(next["method"], "elicitation/create", "{next}");
            message = next["params"]["message"].as_str().unwrap().to_string();
        }
        // The form's text escapes `_` as \u005f.
        assert!(
            message.contains("apple\\u005fitem\\u005fcount"),
            "{mode}: {message}"
        );
        assert!(message.contains("trash"), "{message}");
        assert!(writes(&f.remote).await.is_empty());
        f.server.handle(json!({"jsonrpc":"2.0","id":"x","method":"notifications/cancelled","params":{"requestId":7}})).await;
        // Without form support there is no way to approve it here.
        let mut plain = Server::open(f.dir.path()).unwrap();
        init(&mut plain, false).await;
        let reply = call(&mut plain, json!({"action":"trash","id":REPORT})).await;
        assert_eq!(text(&reply)["error"], "approval_requires_form_client");
    }
}

#[tokio::test]
async fn an_approval_is_stale_after_a_mode_change_or_a_new_cache() {
    let mut f = fixture(None).await;
    let first = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":REPORT})).await);
    f.server.set_policy("yolo").unwrap();
    assert_eq!(
        f.server.approve_any(&first).await,
        Err("approval_policy_changed")
    );
    let second = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":REPORT})).await);
    {
        // The cached item changes after the request was made.
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut item = library.drive_item(REPORT).unwrap().unwrap();
        item.etag = Some("e9".into());
        let mut all = library.drive_children(ROOT).unwrap();
        all.retain(|i| i.drivewsid != REPORT);
        all.push(item);
        library.put_drive_children(ROOT, &all).unwrap();
    }
    assert_eq!(
        f.server.approve_any(&second).await,
        Err("approval_stale_drive_changed_ask_again")
    );
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn a_signed_in_account_other_than_the_folders_changes_nothing() {
    let mut f = fixture(None).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag("999"))
            .unwrap();
    }
    serve_listing(&f.remote, ROOT, root_items(), None).await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","id":REPORT,"name":"x.pdf"})
        )
        .await,
        "account_mismatch_use_new_state_directory"
    );
    assert!(writes(&f.remote).await.is_empty());
    // A terminal request made for one account is refused for another.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
    }
    let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":REPORT})).await);
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag("999"))
            .unwrap();
    }
    assert_eq!(f.server.approve_any(&id).await, Err("account_mismatch"));
}

#[tokio::test]
async fn local_paths_outside_the_folder_links_and_absolute_paths_are_refused() {
    let mut f = fixture(None).await;
    std::fs::write(f.dir.path().parent().unwrap().join("outside.txt"), b"x").ok();
    std::fs::create_dir(f.exchange.join("in")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", f.exchange.join("link.txt")).unwrap();
    std::os::unix::fs::symlink("/etc", f.exchange.join("dirlink")).unwrap();
    std::os::unix::fs::symlink("/tmp", f.exchange.join("in").join("deeper")).unwrap();
    std::fs::write(f.exchange.join("in").join("real.txt"), b"x").unwrap();
    std::fs::hard_link(
        f.exchange.join("in").join("real.txt"),
        f.exchange.join("hard.txt"),
    )
    .unwrap();
    for (local, code) in [
        ("/etc/passwd", "absolute_local_path_refused"),
        ("../outside.txt", "local_path_traversal_refused"),
        ("in/../../outside.txt", "local_path_traversal_refused"),
        ("link.txt", "local_symlink_refused"),
        ("dirlink/passwd", "local_symlink_refused"),
        ("in/deeper/x", "local_symlink_refused"),
        ("hard.txt", "local_hard_link_refused"),
        ("in", "local_not_a_regular_file"),
    ] {
        assert_eq!(
            error(
                &mut f.server,
                json!({"action":"upload","path":"/","local":local})
            )
            .await,
            code,
            "upload {local}"
        );
    }
    for (local, code) in [
        ("/tmp/x", "absolute_local_path_refused"),
        ("../x", "local_path_traversal_refused"),
        ("dirlink/x", "local_symlink_refused"),
        ("link.txt", "local_file_exists_not_overwritten"),
        ("missing/x", "local_folder_not_found"),
    ] {
        assert_eq!(
            error(
                &mut f.server,
                json!({"action":"download","id":REPORT,"local":local})
            )
            .await,
            code,
            "download {local}"
        );
    }
    assert_eq!(
        std::fs::read_link(f.exchange.join("link.txt")).unwrap(),
        Path::new("/etc/passwd")
    );
    assert!(
        requests(&f.remote).await.is_empty(),
        "nothing reached Apple"
    );
}

#[tokio::test]
async fn the_exchange_folder_must_be_set_up_in_the_terminal_and_stay_the_same() {
    let mut f = fixture(None).await;
    // Swapped for another folder of the same name: refused.
    let old = f.exchange.with_extension("old");
    std::fs::rename(&f.exchange, &old).unwrap();
    std::fs::create_dir(&f.exchange).unwrap();
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "exchange_folder_changed_run_skykeep_mcp_exchange"
    );
    std::fs::remove_dir_all(&old).unwrap();
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library.set_state("mcp.drive.exchange", "").unwrap();
    }
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal"
    );
    // No argument names the folder or the limit.
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"download","id":REPORT,"exchange":"/tmp"})
        )
        .await,
        "invalid_arguments"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/","local":"a","max_bytes":1})
        )
        .await,
        "invalid_arguments"
    );
}

async fn serve_download(remote: &MockServer, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path_regex("^/ws/com.apple.CloudDocs/download/by_id$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data_token":{"url":format!("{}/bytes/1", remote.uri())}})),
        )
        .mount(remote)
        .await;
    Mock::given(method("GET"))
        .and(path("/bytes/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(remote)
        .await;
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".skykeep"))
        .collect()
}

#[tokio::test]
async fn download_writes_a_new_file_and_never_overwrites() {
    let mut f = fixture(None).await;
    serve_download(&f.remote, b"hello world".to_vec()).await;
    let done = ok(&mut f.server, json!({"action":"download","id":REPORT})).await;
    assert_eq!(done["local_file"], "report.pdf", "{done}");
    assert_eq!(done["bytes"], 11);
    assert_eq!(
        std::fs::read(f.exchange.join("report.pdf")).unwrap(),
        b"hello world"
    );
    // Again: the file is there and is kept as it is.
    std::fs::write(f.exchange.join("report.pdf"), b"mine").unwrap();
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "local_file_exists_not_overwritten"
    );
    assert_eq!(
        std::fs::read(f.exchange.join("report.pdf")).unwrap(),
        b"mine"
    );
    // Under another name, in a folder inside.
    std::fs::create_dir(f.exchange.join("in")).unwrap();
    ok(
        &mut f.server,
        json!({"action":"download","id":REPORT,"local":"in/copy.pdf"}),
    )
    .await;
    assert_eq!(
        std::fs::read(f.exchange.join("in/copy.pdf")).unwrap(),
        b"hello world"
    );
    assert!(leftovers(&f.exchange).is_empty());
    assert!(
        writes(&f.remote).await.is_empty(),
        "a download changes nothing at Apple"
    );
    // A folder is not downloaded.
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":DOCS})).await,
        "not_a_file"
    );
}

#[tokio::test]
async fn names_from_apple_are_cleaned_or_refused_as_local_names() {
    let mut f = fixture(None).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut all = library.drive_children(ROOT).unwrap();
        let mut odd = all.iter().find(|i| i.drivewsid == REPORT).unwrap().clone();
        odd.drivewsid = "FILE::com.apple.CloudDocs::ODD".into();
        odd.name = "in\u{202E}voice\u{7}".into();
        let mut slash = odd.clone();
        slash.drivewsid = "FILE::com.apple.CloudDocs::SLASH".into();
        slash.name = "../../x".into();
        slash.extension = None;
        all.push(odd);
        all.push(slash);
        library.put_drive_children(ROOT, &all).unwrap();
    }
    serve_download(&f.remote, b"hello world".to_vec()).await;
    let done = ok(
        &mut f.server,
        json!({"action":"download","id":"FILE::com.apple.CloudDocs::ODD"}),
    )
    .await;
    assert_eq!(done["local_file"], "invoice.pdf", "{done}");
    assert!(!done.to_string().contains('\u{202E}'));
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"download","id":"FILE::com.apple.CloudDocs::SLASH"})
        )
        .await,
        "drive_name_not_usable_locally_give_local"
    );
}

/// A server that sends `total` bytes in chunks with no length given, so only counting the
/// bytes as they come can stop it.
async fn endless(total: usize) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = [0u8; 4096];
                let _ = socket.read(&mut head).await;
                if socket
                    .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let chunk = vec![b'a'; 65536];
                let mut sent = 0;
                while sent < total {
                    let frame = format!("{:x}\r\n", chunk.len());
                    if socket.write_all(frame.as_bytes()).await.is_err()
                        || socket.write_all(&chunk).await.is_err()
                        || socket.write_all(b"\r\n").await.is_err()
                    {
                        return;
                    }
                    sent += chunk.len();
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    at
}

#[tokio::test]
async fn the_size_limit_stops_a_download_while_it_streams() {
    let mut f = fixture(None).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        skykeep_mcp::exchange::set_up(&mut library, None, Some(1)).unwrap();
        // Apple's listing gave no size.
        let mut all = library.drive_children(ROOT).unwrap();
        for item in &mut all {
            if item.drivewsid == REPORT {
                item.size = None;
            }
        }
        library.put_drive_children(ROOT, &all).unwrap();
    }
    let bytes = endless(8 * 1024 * 1024).await;
    Mock::given(method("GET"))
        .and(path("/ws/com.apple.CloudDocs/download/by_id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data_token":{"url":format!("{bytes}/data")}})),
        )
        .mount(&f.remote)
        .await;
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "drive_file_over_exchange_limit"
    );
    assert!(!f.exchange.join("report.pdf").exists());
    assert!(leftovers(&f.exchange).is_empty());
    // A size Apple lists over the limit is refused before anything is asked.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut all = library.drive_children(ROOT).unwrap();
        for item in &mut all {
            if item.drivewsid == REPORT {
                item.size = Some(2 * 1024 * 1024);
            }
        }
        library.put_drive_children(ROOT, &all).unwrap();
    }
    f.remote.reset().await;
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "drive_file_over_exchange_limit"
    );
    // So is a local file over it.
    std::fs::write(f.exchange.join("big.bin"), vec![0u8; 1024 * 1024 + 1]).unwrap();
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/","local":"big.bin"})
        )
        .await,
        "local_file_over_exchange_limit"
    );
    assert!(requests(&f.remote).await.is_empty());
}

fn checksum(bytes: &[u8]) -> String {
    let mut sum = quince_icloud::drive::AppleChecksum::default();
    sum.update(bytes);
    sum.finish()
}

async fn serve_upload(remote: &MockServer, bytes: &[u8], slot_delay: Option<Duration>) {
    let uri = remote.uri();
    answer(
        remote,
        "/ws/com.apple.CloudDocs/upload/web",
        json!([{"url":format!("{uri}/slot"),"document_id":"DOC1"}]),
    )
    .await;
    let mut slot = ResponseTemplate::new(200).set_body_json(json!({"singleFile":{
        "fileChecksum":checksum(bytes),"receipt":"R","referenceChecksum":"C","wrappingKey":"K","size":bytes.len()}}));
    if let Some(delay) = slot_delay {
        slot = slot.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/slot"))
        .respond_with(slot)
        .mount(remote)
        .await;
    answer(
        remote,
        "/ws/com.apple.CloudDocs/update/documents",
        json!({"results":[{"document":{"status":{"status_code":0},"data":{"signature":checksum(bytes)},"size":bytes.len(),"document_id":"DOC1"}}]}),
    )
    .await;
    answer(
        remote,
        "/retrieveItemDetails",
        json!({"items":[{"drivewsid":"FILE::com.apple.CloudDocs::DOC1","parentId":DOCS,"name":"plan","extension":"txt","type":"FILE","size":bytes.len()}]}),
    )
    .await;
}

#[tokio::test]
async fn upload_is_checked_and_a_taken_name_is_a_collision() {
    let mut f = fixture(None).await;
    std::fs::write(f.exchange.join("plan.txt"), b"the plan").unwrap();
    serve_listing(&f.remote, DOCS, docs_items(), None).await;
    serve_upload(&f.remote, b"the plan", None).await;
    let done = ok(
        &mut f.server,
        json!({"action":"upload","path":"/Documents","local":"plan.txt"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], true);
    assert_eq!(done["renamed_by_apple"], false);
    let sent = requests(&f.remote).await;
    let slot = sent.iter().find(|r| r.url.path() == "/slot").unwrap();
    assert!(slot.body.windows(8).any(|w| w == b"the plan"));
    // The private copy is gone.
    let staging = f.dir.path().join("drive-upload");
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    // A name already in the folder is refused, never replaced or added beside.
    std::fs::write(f.exchange.join("notes.txt"), b"x").unwrap();
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"notes.txt"})
        )
        .await,
        "name_taken"
    );
    assert_eq!(
        writes(&f.remote)
            .await
            .iter()
            .filter(|p| p.ends_with("/upload/web"))
            .count(),
        1
    );
}

/// The connection ends while the bytes go up: the outcome is unknown, the private copy goes,
/// and a retry finds the file Apple filed after all and adds no second one.
#[tokio::test]
async fn an_interrupted_upload_is_unknown_and_its_retry_adds_no_copy() {
    let mut f = fixture(None).await;
    f.server.set_drive_seam(seam(
        &f.remote.uri(),
        WriteLimits {
            transfer_timeout: Duration::from_millis(1500),
            ..quick()
        },
    ));
    std::fs::write(f.exchange.join("plan.txt"), b"the plan").unwrap();
    serve_listing(&f.remote, DOCS, docs_items(), Some(1)).await;
    serve_listing(
        &f.remote,
        DOCS,
        json!([
            file(NOTES, DOCS, "notes", "txt", 5),
            file("FILE::com.apple.CloudDocs::DOC1", DOCS, "plan", "txt", 8)
        ]),
        None,
    )
    .await;
    serve_upload(&f.remote, b"the plan", Some(Duration::from_secs(10))).await;
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"plan.txt"})
        )
        .await,
        "drive_write_outcome_unknown_run_drive_refresh_before_retrying"
    );
    let staging = f.dir.path().join("drive-upload");
    assert_eq!(
        std::fs::read_dir(&staging).unwrap().count(),
        0,
        "no private copy is left"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"plan.txt"})
        )
        .await,
        "name_taken"
    );
    assert_eq!(
        writes(&f.remote)
            .await
            .iter()
            .filter(|p| p.ends_with("/upload/web"))
            .count(),
        1,
        "the retry sent nothing"
    );
    assert_eq!(
        std::fs::read(f.exchange.join("plan.txt")).unwrap(),
        b"the plan"
    );
}

#[tokio::test]
async fn writes_need_a_signed_in_folder_and_valid_arguments() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, true).await;
    let reply = call(
        &mut server,
        json!({"action":"create_folder","path":"/","name":"x"}),
    )
    .await;
    assert_eq!(text(&reply)["error"], "login_required");
    for (args, code) in [
        (
            json!({"action":"create_folder","path":"/","name":".x"}),
            "invalid_name",
        ),
        (
            json!({"action":"trash","path":"/"}),
            "top_folder_cannot_be_changed",
        ),
        (json!({"action":"move","path":"/a"}), "invalid_arguments"),
        (
            json!({"action":"upload","path":"/","local":"a/../b"}),
            "local_path_traversal_refused",
        ),
        (json!({"action":"nuke","path":"/a"}), "invalid_arguments"),
    ] {
        let reply = call(&mut server, args.clone()).await;
        assert_eq!(text(&reply)["error"], code, "{args}");
    }
}
