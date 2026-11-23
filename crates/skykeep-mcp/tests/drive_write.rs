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
        transfer_timeout: Some(Duration::from_secs(20)),
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

/// Every approval mode, and a folder without one.
const MODES: [Option<&str>; 4] = [
    Some("reads_and_writes"),
    Some("writes_only"),
    Some("yolo"),
    None,
];

/// The approval card for this change: Approve this time and Deny only, never a terminal
/// request. Under reads_and_writes the card for reading the preview comes first and is
/// approved here.
async fn card(server: &mut Server, args: Value) -> Value {
    let mut form = call(server, args.clone()).await;
    while form["params"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("\"read\\u005fpreview\\u005fonly\": true"))
    {
        form = server.handle(approve(&form)).await.unwrap();
    }
    assert_eq!(form["method"], "elicitation/create", "{args}: {form}");
    assert!(!form.to_string().contains("request_id"), "{form}");
    assert_eq!(choices(&form), ["approve", "deny"], "{args}");
    form
}

/// [`card`], then the call is cancelled, so nothing runs.
async fn asks_on_card(server: &mut Server, args: Value) -> Value {
    let form = card(server, args).await;
    let reply = server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}),
        )
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_cancelled", "{reply}");
    form
}

/// The permissions file sets these Drive actions to the opt-in ask: terminal.
fn terminal_rules(dir: &Path, actions: &[&str]) {
    let mut text = String::from("rules:\n");
    for action in actions {
        text.push_str(&format!("  drive_write.{action}: {{ask: terminal}}\n"));
    }
    rules(dir, &text);
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
    assert!(tool["description"].as_str().unwrap().contains(
        "already_has_this_name and already_in_destination mean the change is already done"
    ));
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
    // Before the change (one read per call): as cached. After: Apple shows the new name.
    serve_listing(&f.remote, ROOT, root_items(), Some(3)).await;
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

/// A move asked again after an earlier one whose outcome was unknown: the cache still has the
/// item in its old folder, but Apple already lists it in the destination. Nothing is sent.
#[tokio::test]
async fn a_move_apple_already_shows_done_says_already_in_destination() {
    let mut f = fixture(None).await;
    mark_read(
        f.dir.path(),
        SMALL,
        vec![cached_file("FILE::com.apple.CloudDocs::IN", "in")],
    );
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
    let reply = call(
        &mut f.server,
        json!({"action":"move","id":SMALL,"destination_id":DOCS}),
    )
    .await;
    let value = text(&reply);
    assert_eq!(value["error"], "already_in_destination", "{reply}");
    assert!(value["instructions"]
        .as_str()
        .unwrap()
        .contains("Nothing to do"));
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn move_of_a_small_folder_runs_and_a_large_one_asks_on_the_card_with_a_preview() {
    let mut f = fixture(None).await;
    // Small and read: one file in it.
    mark_read(
        f.dir.path(),
        SMALL,
        vec![cached_file("FILE::com.apple.CloudDocs::IN", "in")],
    );
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
    // Large: the card shows the preview, and nothing is sent until the person approves it.
    let form = asks_on_card(
        &mut f.server,
        json!({"action":"move","id":BIG,"destination_id":DOCS}),
    )
    .await;
    let message = form["params"]["message"].as_str().unwrap();
    assert!(
        message.contains("apple\\u005fitem\\u005fcount\": 500"),
        "{message}"
    );
    assert!(message.contains(DOCS), "{message}");
    assert!(!message.contains("SYNTHETIC-TOKEN"));
    assert_eq!(writes(&f.remote).await, ["/moveItems"]);
    // Renaming it asks the same way.
    asks_on_card(
        &mut f.server,
        json!({"action":"rename","id":BIG,"name":"Huge"}),
    )
    .await;
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

/// Set to ask: terminal, trash and delete forever wait for `skykeep-mcp approve`; approved,
/// trash moves the item to Recently Deleted only, and delete forever then deletes it from
/// there.
#[tokio::test]
async fn trash_and_delete_forever_set_to_ask_terminal_wait_for_approve_and_differ() {
    for (action, deleted) in [("trash", false), ("delete_forever", true)] {
        let mut f = fixture(None).await;
        terminal_rules(f.dir.path(), &[action]);
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

/// Every mode asks a deletion on the card, with the preview and no "Always allow", and never
/// makes a terminal request. A client without cards gets a refusal that says what to do.
#[tokio::test]
async fn deletions_ask_on_the_card_under_every_mode() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        for action in ["trash", "delete_forever"] {
            let form = asks_on_card(&mut f.server, json!({"action":action,"id":DOCS})).await;
            let message = form["params"]["message"].as_str().unwrap();
            // The form's text escapes `_` as \u005f.
            assert!(
                message.contains("apple\\u005fitem\\u005fcount"),
                "{mode:?}: {message}"
            );
            assert!(
                message.contains(action.split('_').next().unwrap()),
                "{message}"
            );
        }
        assert!(writes(&f.remote).await.is_empty(), "{mode:?}");
        // Without card support there is no way to approve it here, and no request is made.
        let mut plain = Server::open(f.dir.path()).unwrap();
        plain.set_drive_seam(seam(&f.remote.uri(), quick()));
        init(&mut plain, false).await;
        for args in [
            json!({"action":"trash","id":REPORT}),
            json!({"action":"delete_forever","id":REPORT}),
            json!({"action":"move","id":BIG,"destination_id":DOCS}),
            json!({"action":"rename","id":BIG,"name":"Huge"}),
        ] {
            let reply = call(&mut plain, args.clone()).await;
            assert_eq!(reply["result"]["isError"], true, "{mode:?} {args}: {reply}");
            let value = text(&reply);
            assert_eq!(value["error"], "approval_requires_form_client", "{value}");
            let message = value["message"].as_str().unwrap();
            assert!(message.contains("nothing ran"), "{message}");
            assert!(!message.contains("skykeep-mcp approve"), "{message}");
            assert!(!reply.to_string().contains("request_id"), "{reply}");
        }
        assert!(writes(&f.remote).await.is_empty(), "{mode:?}");
    }
}

#[tokio::test]
async fn an_approval_is_stale_after_a_mode_change_or_a_new_cache() {
    let mut f = fixture(None).await;
    terminal_rules(f.dir.path(), &["trash"]);
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
    // A terminal request (asked for with ask: terminal) made for one account is refused for
    // another.
    terminal_rules(f.dir.path(), &["trash"]);
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

/// The folder in permissions.yaml wins over the one set up in the terminal; a bad one there
/// refuses the transfer with what to change, and never falls back to the older folder.
#[tokio::test]
async fn the_exchange_folder_in_permissions_yaml_wins_and_a_bad_one_never_falls_back() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fixture(None).await;
    serve_download(&f.remote, b"hello world".to_vec()).await;
    let other = f.exchange.with_extension("yaml");
    std::fs::create_dir(&other).unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = f.dir.path().join("permissions.yaml");
    let write = |folder: &Path| {
        std::fs::write(
            &file,
            format!("version: 1\nexchange_folder: {}\n", folder.display()),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    write(&other);
    ok(&mut f.server, json!({"action":"download","id":REPORT})).await;
    assert_eq!(
        std::fs::read(other.join("report.pdf")).unwrap(),
        b"hello world"
    );
    assert!(!f.exchange.join("report.pdf").exists());
    std::os::unix::fs::symlink(&f.exchange, other.join("link")).unwrap();
    for (folder, code) in [
        (other.join("gone"), "exchange_folder_missing"),
        (other.join("link"), "exchange_folder_is_a_link"),
        (f.dir.path().to_path_buf(), "exchange_folder_private_state"),
    ] {
        write(&folder);
        let reply = call(
            &mut f.server,
            json!({"action":"download","id":REPORT,"local":"again.pdf"}),
        )
        .await;
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        let body = text(&reply);
        assert_eq!(body["error"], code, "{body}");
        let next = body["instructions"].as_str().unwrap();
        assert!(next.contains("permissions.yaml"), "{next}");
        assert!(!f.exchange.join("again.pdf").exists(), "no fallback");
        assert!(!other.join("again.pdf").exists());
    }
    // A line or file that can't be used refuses downloads and uploads, with the file to fix,
    // and never falls back to the folder used before it (issue 75).
    write(&other);
    std::fs::write(other.join("up.txt"), b"up").unwrap();
    for broken in [
        "version: 1\nexchange_folder: rel/ex\n".to_string(),
        format!(
            "version: 1\nexchange_folder: {}\nbogus_key: 1\n",
            other.display()
        ),
    ] {
        std::fs::write(&file, &broken).unwrap();
        for args in [
            json!({"action":"download","id":REPORT,"local":"again.pdf"}),
            json!({"action":"upload","path":"/","local":"up.txt"}),
        ] {
            let reply = call(&mut f.server, args).await;
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            let body = text(&reply);
            assert_eq!(body["error"], "exchange_folder_file_unusable", "{body}");
            let next = body["instructions"].as_str().unwrap();
            assert!(next.contains("permissions.yaml"), "{next}");
        }
        assert!(!f.exchange.join("again.pdf").exists(), "no fallback");
        assert!(!other.join("again.pdf").exists(), "no fallback");
    }
    // The line taken out: no exchange folder, not the one set up in the terminal.
    std::fs::write(&file, "version: 1\n").unwrap();
    let reply = call(
        &mut f.server,
        json!({"action":"download","id":REPORT,"local":"again.pdf"}),
    )
    .await;
    assert_eq!(
        text(&reply)["error"],
        "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal",
        "{reply}"
    );
    assert!(!f.exchange.join("again.pdf").exists(), "no fallback");
    assert!(
        !writes(&f.remote).await.iter().any(|w| w.contains("upload")),
        "nothing was uploaded"
    );
    std::fs::remove_dir_all(&other).unwrap();
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
            transfer_timeout: Some(Duration::from_millis(1500)),
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

/// The upload action sends a file of several pieces through the session's uploads client and
/// the watched upload (#59): Apple holds its receipt back for two seconds once every byte is
/// in, and the file still lands whole, sent once, its receipt checked against those bytes.
#[tokio::test]
async fn a_file_of_many_pieces_goes_through_the_uploads_client_whole() {
    let mut f = fixture(None).await;
    let bytes: Vec<u8> = (0..(1024 * 1024 + 7)).map(|i| (i % 251) as u8).collect();
    std::fs::write(f.exchange.join("plan.txt"), &bytes).unwrap();
    serve_listing(&f.remote, DOCS, docs_items(), None).await;
    serve_upload(&f.remote, &bytes, Some(Duration::from_secs(2))).await;
    let done = ok(
        &mut f.server,
        json!({"action":"upload","path":"/Documents","local":"plan.txt"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], true);
    assert_eq!(done["item"]["size"], bytes.len());
    let sent = requests(&f.remote).await;
    let slots: Vec<_> = sent.iter().filter(|r| r.url.path() == "/slot").collect();
    assert_eq!(slots.len(), 1, "the bytes went once");
    let body = &slots[0].body;
    let at = body
        .windows(64)
        .position(|w| w == &bytes[..64])
        .expect("the file's first bytes are in the upload");
    assert_eq!(
        &body[at..at + bytes.len()],
        &bytes[..],
        "the whole file, in order"
    );
    let staging = f.dir.path().join("drive-upload");
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
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

fn cached_file(id: &str, name: &str) -> quince_core::drive::DriveItem {
    quince_core::drive::DriveItem {
        drivewsid: id.into(),
        parent_id: None,
        name: name.into(),
        extension: Some("txt".into()),
        kind: "FILE".into(),
        size: Some(1),
        child_count: None,
        created_at: None,
        modified_at: None,
        etag: Some("e1".into()),
    }
}

fn cached_folder(id: &str, name: &str, count: i64) -> quince_core::drive::DriveItem {
    quince_core::drive::DriveItem {
        kind: "FOLDER".into(),
        extension: None,
        size: None,
        child_count: Some(count),
        ..cached_file(id, name)
    }
}

/// The cache as if drive_refresh had read `folder` and found `items`.
fn mark_read(dir: &Path, folder: &str, items: Vec<quince_core::drive::DriveItem>) {
    let mut library = quince_core::Library::open(dir).unwrap();
    library.put_drive_children(folder, &items).unwrap();
    let mut listed: Value =
        serde_json::from_str(&library.state("mcp.drive.listed").unwrap().unwrap()).unwrap();
    listed[folder] = json!(1);
    library
        .set_state("mcp.drive.listed", &listed.to_string())
        .unwrap();
}

fn listed(dir: &Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    serde_json::from_str(&library.state("mcp.drive.listed").unwrap().unwrap()).unwrap()
}

/// A folder whose contents were never read counts as large: its move or rename waits for the
/// person. So does a folder read whole but with a folder beneath it never read.
#[tokio::test]
async fn a_folder_not_read_whole_counts_as_many_items() {
    let mut f = fixture(None).await;
    // Small: Apple counts one item, never read.
    asks_on_card(
        &mut f.server,
        json!({"action":"rename","id":SMALL,"name":"Tiny"}),
    )
    .await;
    // Read, but the folder in it never was.
    mark_read(
        f.dir.path(),
        SMALL,
        vec![cached_folder("FOLDER::com.apple.CloudDocs::SUB", "Sub", 3)],
    );
    asks_on_card(
        &mut f.server,
        json!({"action":"move","id":SMALL,"destination_id":DOCS}),
    )
    .await;
    // An empty folder is known to hold nothing.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut all = library.drive_children(ROOT).unwrap();
        for item in &mut all {
            if item.drivewsid == SMALL {
                item.child_count = Some(0);
            }
        }
        library.put_drive_children(ROOT, &all).unwrap();
        library.put_drive_children(SMALL, &[]).unwrap();
    }
    serve_listing(&f.remote, ROOT, root_items(), None).await;
    answer(&f.remote, "/renameItems", ok_items(SMALL)).await;
    let reply = call(
        &mut f.server,
        json!({"action":"rename","id":SMALL,"name":"Tiny"}),
    )
    .await;
    assert_ne!(reply["method"], "elicitation/create", "{reply}");
    assert!(writes(&f.remote)
        .await
        .contains(&"/renameItems".to_string()));
}

/// A rename asked again after it worked says so and sends nothing, though Apple's version of
/// the item moved on with the rename.
#[tokio::test]
async fn a_rename_repeated_after_it_worked_says_already_has_this_name() {
    let mut f = fixture(None).await;
    let mut renamed = root_items();
    renamed[3]["name"] = json!("final");
    renamed[3]["etag"] = json!("e2");
    serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
    serve_listing(&f.remote, ROOT, renamed, None).await;
    answer(&f.remote, "/renameItems", ok_items(REPORT)).await;
    let done = ok(
        &mut f.server,
        json!({"action":"rename","id":REPORT,"name":"final.pdf"}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","id":REPORT,"name":"final.pdf"})
        )
        .await,
        "already_has_this_name"
    );
    assert_eq!(writes(&f.remote).await, ["/renameItems"]);
}

/// A trashed folder takes the folders read beneath it out of the read record, as a refresh
/// that finds a folder gone does.
#[tokio::test]
async fn trashing_a_folder_forgets_the_folders_read_beneath_it() {
    let mut f = fixture(None).await;
    const SUB: &str = "FOLDER::com.apple.CloudDocs::SUB";
    mark_read(
        f.dir.path(),
        DOCS,
        vec![cached_file(NOTES, "notes"), cached_folder(SUB, "Sub", 0)],
    );
    mark_read(f.dir.path(), SUB, vec![]);
    assert!(listed(f.dir.path()).get(SUB).is_some());
    let form = call(&mut f.server, json!({"action":"trash","id":DOCS})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
    serve_listing(&f.remote, ROOT, json!([folder(BIG, "Big", 500)]), None).await;
    answer(&f.remote, "/moveItemsToTrash", ok_items(DOCS)).await;
    let done = text(&f.server.handle(approve(&form)).await.unwrap());
    assert_eq!(done["outcome"], "done", "{done}");
    let now = listed(f.dir.path());
    assert!(now.get(SUB).is_none(), "{now}");
    assert!(now.get(DOCS).is_none(), "{now}");
    assert!(now.get(ROOT).is_none(), "the top folder changed: {now}");
}

/// A download gets the version previewed, or nothing.
#[tokio::test]
async fn a_download_of_a_file_changed_since_it_was_cached_is_refused() {
    let mut f = fixture(None).await;
    Mock::given(method("GET"))
        .and(path("/ws/com.apple.CloudDocs/download/by_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"double_etag":"e2","data_token":{"url":format!("{}/bytes/1", f.remote.uri())}}),
        ))
        .mount(&f.remote)
        .await;
    assert_eq!(
        error(&mut f.server, json!({"action":"download","id":REPORT})).await,
        "drive_item_changed_run_drive_refresh"
    );
    assert!(!f.exchange.join("report.pdf").exists());
    assert!(leftovers(&f.exchange).is_empty());
    assert!(!requests(&f.remote)
        .await
        .iter()
        .any(|r| r.url.path() == "/bytes/1"));
}

/// A partial download a crash left behind goes at the next download into its folder, once it
/// is old enough that nothing is still writing it; a fresh one stays.
#[tokio::test]
async fn stale_partial_downloads_are_removed_by_the_next_download() {
    let mut f = fixture(None).await;
    let old = f.exchange.join(".skykeep-download-old.part");
    let young = f.exchange.join(".skykeep-download-young.part");
    let other = f.exchange.join(".skykeep-download-mine.txt");
    for p in [&old, &young, &other] {
        std::fs::write(p, b"x").unwrap();
    }
    let past = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(past)
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&other)
        .unwrap()
        .set_modified(past)
        .unwrap();
    serve_download(&f.remote, b"hello world".to_vec()).await;
    ok(&mut f.server, json!({"action":"download","id":REPORT})).await;
    assert!(!old.exists());
    assert!(young.exists());
    assert!(other.exists(), "only partial downloads are removed");
}

/// A folder inside the exchange folder that others can write to is refused, both ways.
#[tokio::test]
async fn a_shared_folder_inside_the_exchange_folder_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fixture(None).await;
    let shared = f.exchange.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("a.txt"), b"x").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"upload","path":"/","local":"shared/a.txt"})
        )
        .await,
        "local_folder_not_private"
    );
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"download","id":REPORT,"local":"shared/b.pdf"})
        )
        .await,
        "local_folder_not_private"
    );
    assert!(requests(&f.remote).await.is_empty());
}

/// Without an exchange folder, the error says the exact command, with this private folder.
#[tokio::test]
async fn the_missing_exchange_folder_error_names_the_command_to_type() {
    let mut f = fixture(None).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library.set_state("mcp.drive.exchange", "").unwrap();
    }
    let reply = call(&mut f.server, json!({"action":"download","id":REPORT})).await;
    let value = text(&reply);
    assert_eq!(
        value["error"],
        "exchange_folder_not_set_up_run_skykeep_mcp_exchange_in_a_terminal"
    );
    let instructions = value["instructions"].as_str().unwrap();
    let dir = f.dir.path().canonicalize().unwrap();
    assert!(
        instructions.contains(&format!(
            "skykeep-mcp exchange --state-dir {}",
            dir.display()
        )),
        "{instructions}"
    );
}

/// What `skykeep-mcp exchange` prints when it stops: what is wrong and what to type.
#[test]
fn exchange_setup_errors_say_what_to_type() {
    use skykeep_mcp::exchange::{explain, set_up};
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir();
    let state = dir.path();
    let range = explain("exchange_limit_out_of_range", None, state);
    assert!(range.contains("from 1 to 10240"), "{range}");
    assert!(range.contains("--max-mb 100"), "{range}");
    let mut library = quince_core::Library::open(state).unwrap();
    let missing = state.join("no/such/Exchange");
    assert_eq!(
        set_up(&mut library, Some(missing.clone()), None),
        Err("exchange_parent_missing")
    );
    let text = explain("exchange_parent_missing", Some(&missing), state);
    assert!(
        text.contains(&format!("mkdir -p {}", state.join("no/such").display())),
        "{text}"
    );
    let open = state.join("open folder");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        set_up(&mut library, Some(open.clone()), None),
        Err("exchange_folder_writable_by_others")
    );
    let text = explain("exchange_folder_writable_by_others", Some(&open), state);
    assert!(
        text.contains(&format!("chmod 700 '{}'", open.display())),
        "{text}"
    );
    assert_eq!(
        set_up(&mut library, Some(state.join("fine")), Some(0)),
        Err("exchange_limit_out_of_range")
    );
    assert_eq!(
        set_up(&mut library, Some(state.join("fine")), Some(10_241)),
        Err("exchange_limit_out_of_range")
    );
    assert_eq!(
        set_up(&mut library, Some(state.join("fine")), Some(10_240))
            .unwrap()
            .max_bytes,
        10_240 * 1024 * 1024
    );
    assert_eq!(
        skykeep_mcp::status::with_dir("run skykeep-mcp exchange", state),
        format!(
            "run skykeep-mcp exchange --state-dir {}",
            skykeep_mcp_quoted(state)
        )
    );
}

fn skykeep_mcp_quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@".contains(c))
    {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[test]
fn transfers_get_more_time_under_a_larger_limit() {
    use skykeep_mcp::drive_write::transfer_time;
    assert_eq!(transfer_time(100 * 1024 * 1024), Duration::from_secs(520));
    assert!(transfer_time(10_240 * 1024 * 1024) > transfer_time(100 * 1024 * 1024));
}

/// An upload also gets the time Apple may take to answer once every byte is in, as a Photos
/// upload does; a download does not wait on such an answer and keeps the plain budget.
#[test]
fn an_upload_gets_apples_answer_time_on_top_of_the_transfer() {
    use skykeep_mcp::drive_write::{transfer_time, upload_time};
    assert_eq!(
        upload_time(100 * 1024 * 1024),
        Duration::from_secs(520 + 300)
    );
    assert_eq!(
        upload_time(1) - transfer_time(1),
        quince_icloud::upload::ANSWER
    );
}

/// delete_forever after the move to Recently Deleted: Apple's answer decides what the error
/// says. Busy (or a dropped connection) leaves the outcome unknown; a refusal that names the
/// item as kept says it is in Recently Deleted, with what to do next.
async fn delete_forever_after_trash(final_answer: ResponseTemplate) -> Result<Value, &'static str> {
    let mut f = fixture(None).await;
    terminal_rules(f.dir.path(), &["delete_forever"]);
    let id = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"delete_forever","id":REPORT}),
        )
        .await,
    );
    serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
    serve_listing(&f.remote, ROOT, json!([folder(DOCS, "Documents", 2)]), None).await;
    answer(&f.remote, "/moveItemsToTrash", ok_items(REPORT)).await;
    serve_listing(
        &f.remote,
        "TRASH_ROOT",
        json!([file(REPORT, "TRASH_ROOT", "report", "pdf", 11)]),
        None,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/deleteItems"))
        .respond_with(final_answer)
        .mount(&f.remote)
        .await;
    f.server.approve_any(&id).await
}

#[tokio::test]
async fn a_busy_apple_after_the_trash_step_leaves_delete_forever_unknown() {
    let error = delete_forever_after_trash(
        ResponseTemplate::new(503).set_body_json(json!({"retryAfter":1})),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        "drive_write_outcome_unknown_run_drive_refresh_before_retrying"
    );
    let step = skykeep_mcp::next_step(error, Path::new("/state"), &Value::Null).unwrap();
    assert!(step.contains("Run drive_refresh"), "{step}");
}

#[tokio::test]
async fn a_refused_final_delete_says_the_item_is_in_recently_deleted_and_what_to_do() {
    let error = delete_forever_after_trash(
        ResponseTemplate::new(200)
            .set_body_json(json!({"items":[{"drivewsid":REPORT,"status":"FORBIDDEN"}]})),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        "drive_trashed_not_deleted_forever_it_is_in_recently_deleted"
    );
    let step = skykeep_mcp::next_step(error, Path::new("/state"), &Value::Null).unwrap();
    assert!(
        step.contains("Recently Deleted") && step.contains("30 days"),
        "{step}"
    );
    assert!(skykeep_mcp::next_step(
        "drive_trashed_not_deleted_forever_login_required",
        Path::new("/state"),
        &Value::Null
    )
    .unwrap()
    .contains("skykeep-mcp login --state-dir /state"));
}

/// The approval form path carries the next step in the error result too.
#[tokio::test]
async fn a_form_approved_delete_forever_that_stops_half_way_says_what_to_do() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        json!({"action":"delete_forever","id":REPORT}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    serve_listing(&f.remote, ROOT, root_items(), Some(2)).await;
    serve_listing(&f.remote, ROOT, json!([folder(DOCS, "Documents", 2)]), None).await;
    answer(&f.remote, "/moveItemsToTrash", ok_items(REPORT)).await;
    serve_listing(
        &f.remote,
        "TRASH_ROOT",
        json!([file(REPORT, "TRASH_ROOT", "report", "pdf", 11)]),
        None,
    )
    .await;
    answer(
        &f.remote,
        "/deleteItems",
        json!({"items":[{"drivewsid":REPORT,"status":"FORBIDDEN"}]}),
    )
    .await;
    let reply = f
        .server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}))
        .await
        .unwrap();
    let value = text(&reply);
    assert_eq!(
        value["error"], "drive_trashed_not_deleted_forever_it_is_in_recently_deleted",
        "{reply}"
    );
    assert!(value["instructions"]
        .as_str()
        .unwrap()
        .contains("Recently Deleted"));
}

/// yolo says what it still asks.
#[test]
fn the_yolo_description_says_drive_deletions_still_ask_on_the_card() {
    let yolo = skykeep_mcp::status::describe_mode("yolo");
    assert!(yolo.contains("Deletions"), "{yolo}");
    assert!(
        yolo.contains("moves and renames of large Drive folders"),
        "{yolo}"
    );
    assert!(yolo.contains("ask on the approval card"), "{yolo}");
    assert!(!yolo.contains("skykeep-mcp approve"), "{yolo}");
    assert!(!yolo.contains("Never ask"), "{yolo}");
}

/// A download says whether the version was checked: not for an empty file, which is never
/// fetched, nor for one cached without a version.
#[tokio::test]
async fn a_download_says_whether_its_version_was_checked() {
    let mut f = fixture(None).await;
    serve_download(&f.remote, b"hello world".to_vec()).await;
    let checked = ok(&mut f.server, json!({"action":"download","id":REPORT})).await;
    assert_eq!(checked["version_checked"], true, "{checked}");
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut all = library.drive_children(ROOT).unwrap();
        for item in &mut all {
            if item.drivewsid == REPORT {
                item.size = Some(0);
            }
        }
        library.put_drive_children(ROOT, &all).unwrap();
    }
    let empty = ok(
        &mut f.server,
        json!({"action":"download","id":REPORT,"local":"empty.pdf"}),
    )
    .await;
    assert_eq!(empty["version_checked"], false, "{empty}");
    assert!(
        empty["checked"].as_str().unwrap().contains("not checked"),
        "{empty}"
    );
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut all = library.drive_children(ROOT).unwrap();
        for item in &mut all {
            if item.drivewsid == REPORT {
                item.size = Some(11);
                item.etag = None;
            }
        }
        library.put_drive_children(ROOT, &all).unwrap();
    }
    let unversioned = ok(
        &mut f.server,
        json!({"action":"download","id":REPORT,"local":"plain.pdf"}),
    )
    .await;
    assert_eq!(unversioned["version_checked"], false, "{unversioned}");
}

/// Apple files an upload under a free name when the name was taken after the folder was read:
/// the reply says so.
#[tokio::test]
async fn an_upload_apple_files_under_another_name_says_renamed_by_apple() {
    let mut f = fixture(None).await;
    std::fs::write(f.exchange.join("plan.txt"), b"the plan").unwrap();
    serve_listing(&f.remote, DOCS, docs_items(), None).await;
    Mock::given(method("POST"))
        .and(path("/retrieveItemDetails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[{"drivewsid":"FILE::com.apple.CloudDocs::DOC1","parentId":DOCS,"name":"plan 2","extension":"txt","type":"FILE","size":8}]})))
        .with_priority(1)
        .mount(&f.remote)
        .await;
    serve_upload(&f.remote, b"the plan", None).await;
    let done = ok(
        &mut f.server,
        json!({"action":"upload","path":"/Documents","local":"plan.txt"}),
    )
    .await;
    assert_eq!(done["renamed_by_apple"], true, "{done}");
    assert_eq!(done["item"]["name"], "plan 2.txt");
}

/// The terminal approval shows a Drive change in plain lines, and never passes on a
/// character that reorders or hides text.
#[tokio::test]
async fn the_terminal_approval_text_is_plain_and_inert() {
    let mut f = fixture(None).await;
    terminal_rules(f.dir.path(), &["move", "trash"]);
    let id = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"move","id":BIG,"destination_id":DOCS}),
        )
        .await,
    );
    let text = f.server.approval_text(&id).unwrap();
    assert!(!text.trim_start().starts_with('{'), "{text}");
    assert!(text.contains("Drive change: move"), "{text}");
    assert!(text.contains("Item: Big (/Big)"), "{text}");
    assert!(text.contains("Move into: Documents (/Documents)"), "{text}");
    assert!(text.contains("500 items directly"), "{text}");
    let trash = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":REPORT})).await);
    let text = f.server.approval_text(&trash).unwrap();
    assert!(
        text.contains("a deletion") && text.contains("Recently Deleted"),
        "{text}"
    );
    // A queued Notes change keeps its JSON, with direction characters made visible.
    let other = "6f1c2a8e-7f55-4c9a-9d1e-0b5a3c2d1e4f";
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        library
            .set_state(
                &format!("mcp.approval.{other}"),
                &json!({"tool":"notes_write","args":{"action":"delete","guid":"N\u{202E}1"},"account":"x"}).to_string(),
            )
            .unwrap();
    }
    let mut reopened = Server::open(f.dir.path()).unwrap();
    reopened.set_drive_seam(seam(&f.remote.uri(), quick()));
    let text = reopened.approval_text(other).unwrap();
    assert!(!text.contains('\u{202E}'), "{text}");
    assert!(text.contains("N\u{FFFD}1"), "{text}");
}

/// The refusal for a shared folder inside the exchange folder names the chmod line.
#[tokio::test]
async fn a_shared_folder_refusal_names_the_fix() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fixture(None).await;
    let shared = f.exchange.join("in").join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("a.txt"), b"x").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let reply = call(
        &mut f.server,
        json!({"action":"upload","path":"/","local":"in/shared/a.txt"}),
    )
    .await;
    let value = text(&reply);
    assert_eq!(value["error"], "local_folder_not_private");
    let instructions = value["instructions"].as_str().unwrap();
    assert!(
        instructions.contains(&format!("chmod 700 {}", shared.display())),
        "{instructions}"
    );
}

/// `skykeep-mcp exchange` without --state-dir uses the default private folder, as setup does:
/// run in a terminal of its own, it sets up both folders and prints the exchange folder.
#[test]
fn the_exchange_command_defaults_its_state_folder() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    let home = private_dir();
    std::fs::create_dir_all(home.path().join(".local/share")).unwrap();
    let (mut master, slave) = {
        let (mut m, mut s) = (0, 0);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s)) }
    };
    let tty = slave.as_raw_fd();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_skykeep-mcp"));
    command
        .args(["exchange", "--max-mb", "5"])
        .env("HOME", home.path())
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    for marker in skykeep_mcp::terminal::AGENT_MARKERS {
        command.env_remove(marker);
    }
    // The terminal becomes the command's own, as a terminal window's is.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(tty, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // Every copy of the terminal's far end closes here, so reading ends when the command does.
    drop(command);
    drop(slave);
    let mut said = Vec::new();
    let mut buf = [0u8; 1024];
    while let Ok(n) = master.read(&mut buf) {
        if n == 0 {
            break;
        }
        said.extend_from_slice(&buf[..n]);
    }
    let status = child.wait().unwrap();
    let said = String::from_utf8_lossy(&said);
    assert!(status.success(), "{said}");
    let exchange = home.path().join("Skykeep-Exchange");
    let real = std::fs::canonicalize(home.path())
        .unwrap()
        .join("Skykeep-Exchange");
    assert!(
        said.contains(&format!("Drive exchange folder: {}", real.display())),
        "{said}"
    );
    assert!(said.contains("Size limit: 5 MB"), "{said}");
    assert!(exchange.is_dir());
    let state = home.path().join(".local/share/skykeep-mcp");
    let library = quince_core::Library::open(&state).unwrap();
    let saved: Value =
        serde_json::from_str(&library.state("mcp.drive.exchange").unwrap().unwrap()).unwrap();
    assert_eq!(saved["max_bytes"], 5 * 1024 * 1024);
    // The folder is the file's setting now; the command said so.
    assert_eq!(
        skykeep_mcp::permissions::setting_in_file(
            &state.canonicalize().unwrap(),
            skykeep_mcp::permissions::EXCHANGE_KEY
        )
        .as_deref(),
        Some(real.to_str().unwrap())
    );
    assert!(
        said.contains("permissions.yaml now says exchange_folder:"),
        "{said}"
    );
}

/// A link among the folders above the exchange folder (as /home can be on some systems) is
/// followed once at setup and the real path kept; the exchange folder itself may not be one,
/// and the refusal names the real folder to use instead.
#[test]
fn a_symbolic_link_above_the_exchange_folder_is_resolved_at_setup() {
    use skykeep_mcp::exchange::{explain, set_up};
    let dir = private_dir();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let real = dir.path().join("real home");
    std::fs::create_dir(&real).unwrap();
    let linked = dir.path().join("home");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let mut library = quince_core::Library::open(&state).unwrap();
    let saved = set_up(&mut library, Some(linked.join("Exchange")), None).unwrap();
    let real_exchange = std::fs::canonicalize(&real).unwrap().join("Exchange");
    assert_eq!(saved.path, real_exchange);
    assert!(real_exchange.is_dir());
    // The exchange folder itself as a link: refused, with the real folder to name.
    let link_folder = dir.path().join("link-exchange");
    std::os::unix::fs::symlink(&real_exchange, &link_folder).unwrap();
    assert_eq!(
        set_up(&mut library, Some(link_folder.clone()), None),
        Err("exchange_folder_path_unsafe")
    );
    let text = explain("exchange_folder_path_unsafe", Some(&link_folder), &state);
    assert!(
        text.contains(&format!("--path '{}'", real_exchange.display())),
        "{text}"
    );
}

/// The terminal approval names an upload's name in iCloud Drive and the local name a
/// download takes when none is given.
#[tokio::test]
async fn the_approval_text_names_both_ends_of_a_transfer() {
    let mut f = fixture(None).await;
    terminal_rules(f.dir.path(), &["upload", "download"]);
    let up = proposal_id(
        &ok(
            &mut f.server,
            json!({"action":"upload","path":"/Documents","local":"in/plan.txt","name":"Plan.txt"}),
        )
        .await,
    );
    let text = f.server.approval_text(&up).unwrap();
    assert!(text.contains("Name in iCloud Drive: Plan.txt"), "{text}");
    assert!(
        text.contains("Local file in the exchange folder: in/plan.txt"),
        "{text}"
    );
    let down = proposal_id(&ok(&mut f.server, json!({"action":"download","id":REPORT})).await);
    let text = f.server.approval_text(&down).unwrap();
    assert!(
        text.contains("Local file in the exchange folder: report.pdf"),
        "{text}"
    );
}

/// After a refusal, approve prints what to do next.
#[test]
fn common_refusals_say_what_to_do_next() {
    for code in [
        "name_taken",
        "local_file_exists_not_overwritten",
        "already_has_this_name",
        "already_in_destination",
    ] {
        let step = skykeep_mcp::next_step(code, Path::new("/state"), &Value::Null);
        assert!(step.as_deref().is_some_and(|s| s.len() > 20), "{code}");
    }
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

fn approve(form: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}})
}

/// The answers a rename of the report needs: as cached before, renamed after.
async fn serve_rename(remote: &MockServer) {
    serve_listing(remote, ROOT, root_items(), Some(1)).await;
    serve_listing(
        remote,
        ROOT,
        json!([
            file(REPORT, ROOT, "final", "pdf", 11),
            folder(DOCS, "Documents", 2)
        ]),
        None,
    )
    .await;
    answer(remote, "/renameItems", ok_items(REPORT)).await;
}

/// The answers a trash of the report needs.
async fn serve_trash(remote: &MockServer) {
    serve_listing(remote, ROOT, root_items(), Some(2)).await;
    serve_listing(remote, ROOT, json!([folder(DOCS, "Documents", 2)]), None).await;
    answer(remote, "/moveItemsToTrash", ok_items(REPORT)).await;
}

#[test]
fn drive_write_actions_are_rule_names_and_deletions_are_decided_in_one_place() {
    use skykeep_mcp::permissions::{self, Removal};
    let names = permissions::names();
    assert!(names.iter().any(|n| n == "drive_write"));
    for action in skykeep_mcp::drive_write::ACTIONS {
        assert!(
            names.iter().any(|n| *n == format!("drive_write.{action}")),
            "{action}"
        );
        // Pinned: only trash and delete_forever remove something.
        let removes = matches!(action, "trash" | "delete_forever");
        assert_eq!(
            skykeep_mcp::drive_write::is_destructive(action),
            removes,
            "{action}"
        );
        assert_eq!(
            permissions::is_deletion("drive_write", action),
            removes,
            "{action}"
        );
    }
    assert_eq!(skykeep_mcp::drive_write::ACTIONS.len(), 7);
    assert_eq!(
        permissions::removal_of("drive_write", "trash"),
        Removal::Recoverable
    );
    assert_eq!(
        permissions::removal_of("drive_write", "delete_forever"),
        Removal::Permanent
    );
    assert!(permissions::parse("rules:\n  drive_write.delete_forever: deny\n").is_ok());
    let starter = permissions::parse(&permissions::starter(Path::new("/srv/sky"))).unwrap();
    assert!(
        starter.contains_key("drive_write.trash")
            && starter.contains_key("drive_write.delete_forever")
    );
}

#[tokio::test]
async fn deny_refuses_drive_changes_and_their_waiting_terminal_requests() {
    let mut f = fixture(None).await;
    rules(f.dir.path(), "rules:\n  drive_write.rename: deny\n");
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"rename","path":"/report.pdf","name":"final.pdf"})
        )
        .await,
        "denied_by_permissions"
    );
    rules(f.dir.path(), "rules:\n  drive_write: deny\n");
    assert_eq!(
        error(
            &mut f.server,
            json!({"action":"create_folder","path":"/New"})
        )
        .await,
        "denied_by_permissions"
    );
    terminal_rules(f.dir.path(), &["trash"]);
    let id = proposal_id(&ok(&mut f.server, json!({"action":"trash","id":REPORT})).await);
    rules(f.dir.path(), "rules:\n  drive_write.trash: deny\n");
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("denied_by_permissions")
    );
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn allow_runs_a_small_drive_change_unasked_but_never_a_deletion_or_large_move() {
    let mut f = fixture(Some("writes_only")).await;
    let rename = json!({"action":"rename","path":"/report.pdf","name":"final.pdf"});
    let form = call(&mut f.server, rename.clone()).await;
    assert_eq!(
        form["method"], "elicitation/create",
        "without a rule the mode asks"
    );
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    f.server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
        .await;
    rules(f.dir.path(), "rules:\n  drive_write: allow\n");
    serve_rename(&f.remote).await;
    let done = ok(&mut f.server, rename).await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(writes(&f.remote).await, ["/renameItems"]);
    // A deletion and a large folder move still ask, and the card offers no Always allow.
    for args in [
        json!({"action":"trash","id":REPORT}),
        json!({"action":"delete_forever","id":REPORT}),
        json!({"action":"move","id":BIG,"destination_id":DOCS}),
    ] {
        let form = call(&mut f.server, args.clone()).await;
        assert_eq!(form["method"], "elicitation/create", "{args}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"], "{args}");
        f.server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
            .await;
    }
    assert_eq!(writes(&f.remote).await, ["/renameItems"]);
    // Under yolo and without a mode they ask on the card too.
    for mode in [Some("yolo"), None] {
        let mut f = fixture(mode).await;
        rules(f.dir.path(), "rules:\n  drive_write: allow\n");
        for action in ["trash", "delete_forever"] {
            asks_on_card(&mut f.server, json!({"action":action,"id":REPORT})).await;
        }
        asks_on_card(
            &mut f.server,
            json!({"action":"move","id":BIG,"destination_id":DOCS}),
        )
        .await;
        assert!(writes(&f.remote).await.is_empty(), "{mode:?}");
        assert!(!f
            .dir
            .path()
            .join(skykeep_mcp::permissions::LOG_NAME)
            .exists());
    }
}

#[tokio::test]
async fn ask_terminal_for_a_drive_change_uses_its_preview_and_goes_stale_on_an_edit() {
    let mut f = fixture(Some("writes_only")).await;
    let rename = json!({"action":"rename","path":"/report.pdf","name":"final.pdf"});
    rules(
        f.dir.path(),
        "rules:\n  drive_write.rename: {ask: terminal}\n",
    );
    let id = proposal_id(&ok(&mut f.server, rename.clone()).await);
    let details: Value = serde_json::from_str(&f.server.approval_details(&id).unwrap()).unwrap();
    assert!(details["preview"].is_object(), "{details}");
    rules(
        f.dir.path(),
        "# edited\nrules:\n  drive_write.rename: {ask: terminal}\n",
    );
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("approval_policy_changed")
    );
    let id = proposal_id(&ok(&mut f.server, rename).await);
    assert!(writes(&f.remote).await.is_empty());
    serve_rename(&f.remote).await;
    let done = f.server.approve_any(&id).await.unwrap();
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(writes(&f.remote).await, ["/renameItems"]);
}

#[tokio::test]
async fn a_card_approval_of_a_drive_deletion_under_yolo_is_not_asked_again() {
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  drive_write.trash: {ask: client}\n");
    let form = call(&mut f.server, json!({"action":"trash","id":REPORT})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny"]);
    serve_trash(&f.remote).await;
    let reply = f.server.handle(approve(&form)).await.unwrap();
    let done = text(&reply);
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(writes(&f.remote).await, ["/moveItemsToTrash"]);
}

#[tokio::test]
async fn permissions_show_says_drive_deletions_and_large_moves_still_ask() {
    let f = fixture(Some("writes_only")).await;
    rules(
        f.dir.path(),
        "rules:\n  drive_write.trash: allow\n  drive_write.rename: allow\n",
    );
    let shown =
        skykeep_mcp::permissions::show(&f.dir.path().canonicalize().unwrap(), false).unwrap();
    assert!(
        shown.contains("drive_write.trash: allow, but asks in your MCP client, because a deletion always needs a person"),
        "{shown}"
    );
    assert!(
        shown.contains("drive_write.rename: allow, runs without asking, except renaming or moving a large folder, which still asks"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_broken_file_under_yolo_still_asks_drive_deletions_on_the_card() {
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  bogus_name: allow\n");
    for action in ["trash", "delete_forever"] {
        asks_on_card(&mut f.server, json!({"action":action,"id":REPORT})).await;
    }
    assert!(writes(&f.remote).await.is_empty());
}

#[tokio::test]
async fn permissions_show_and_the_history_match_what_runs_for_drive_deletions_under_yolo() {
    let mut f = fixture(Some("yolo")).await;
    let dir = f.dir.path().canonicalize().unwrap();
    rules(
        f.dir.path(),
        "rules:\n  drive_write.trash: {ask: client}\n  drive_write.delete_forever: allow\n",
    );
    call(
        &mut f.server,
        json!({"action":"create_folder","path":"/x/y"}),
    )
    .await;
    let shown = skykeep_mcp::permissions::show(&dir, false).unwrap();
    assert!(
        shown.contains("drive_write.trash: ask: client, asks in your MCP client\n"),
        "{shown}"
    );
    assert!(
        shown.contains("drive_write.delete_forever: allow, but asks in your MCP client, because a deletion always needs a person"),
        "{shown}"
    );
    // Under yolo a trash asks on the card anyway, so the ask: client rule changes nothing.
    assert!(!shown.contains("looser: drive_write.trash"), "{shown}");
    assert!(!shown.contains("skykeep-mcp approve"), "{shown}");
    assert!(
        !shown.contains("drive_write.delete_forever: waits"),
        "{shown}"
    );
}

/// The answers a delete forever of the report needs: the move to Recently Deleted, then the
/// delete from there.
async fn serve_delete_forever(remote: &MockServer) {
    serve_trash(remote).await;
    serve_listing(
        remote,
        "TRASH_ROOT",
        json!([file(REPORT, "TRASH_ROOT", "report", "pdf", 11)]),
        Some(1),
    )
    .await;
    serve_listing(remote, "TRASH_ROOT", json!([]), None).await;
    answer(remote, "/deleteItems", ok_items(REPORT)).await;
}

/// The answers a rename of the large folder needs.
async fn serve_big_rename(remote: &MockServer) {
    serve_listing(remote, ROOT, root_items(), Some(1)).await;
    let mut renamed = root_items();
    renamed[1]["name"] = json!("Huge");
    renamed[1]["etag"] = json!("f2");
    serve_listing(remote, ROOT, renamed, None).await;
    answer(remote, "/renameItems", ok_items(BIG)).await;
}

/// The cached report gets a new version, as a refresh that saw it change would leave it.
fn change_cached_report(dir: &Path) {
    let mut library = quince_core::Library::open(dir).unwrap();
    let mut item = library.drive_item(REPORT).unwrap().unwrap();
    item.etag = Some("e9".into());
    let mut all = library.drive_children(ROOT).unwrap();
    all.retain(|i| i.drivewsid != REPORT);
    all.push(item);
    library.put_drive_children(ROOT, &all).unwrap();
}

/// In every mode, a deletion and a large folder change ask on the card; approved, they run
/// once, and the same answer sent again runs nothing.
#[tokio::test]
async fn card_approvals_run_once_in_every_mode() {
    for mode in MODES {
        for (args, sent) in [
            (
                json!({"action":"trash","id":REPORT}),
                vec!["/moveItemsToTrash"],
            ),
            (
                json!({"action":"delete_forever","id":REPORT}),
                vec!["/moveItemsToTrash", "/deleteItems"],
            ),
            (
                json!({"action":"rename","id":BIG,"name":"Huge"}),
                vec!["/renameItems"],
            ),
        ] {
            let mut f = fixture(mode).await;
            let form = card(&mut f.server, args.clone()).await;
            assert!(writes(&f.remote).await.is_empty(), "{mode:?} {args}");
            match args["action"].as_str().unwrap() {
                "trash" => serve_trash(&f.remote).await,
                "delete_forever" => serve_delete_forever(&f.remote).await,
                _ => serve_big_rename(&f.remote).await,
            }
            let reply = f.server.handle(approve(&form)).await.unwrap();
            assert_eq!(
                reply["result"]["isError"], false,
                "{mode:?} {args}: {reply}"
            );
            assert_eq!(text(&reply)["outcome"], "done", "{mode:?} {args}: {reply}");
            assert_eq!(writes(&f.remote).await, sent, "{mode:?} {args}");
            // A second use of the same approval is no approval.
            assert!(f.server.handle(approve(&form)).await.is_none());
            assert_eq!(writes(&f.remote).await, sent, "{mode:?} {args}");
            assert!(!f
                .dir
                .path()
                .join(skykeep_mcp::permissions::LOG_NAME)
                .exists());
        }
    }
}

/// A card denied, declined, cancelled or left unanswered counts as Deny in every mode: nothing
/// is sent, and the reply says nothing ran.
#[tokio::test]
async fn a_card_denied_declined_cancelled_or_timed_out_runs_nothing() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        serve_trash(&f.remote).await;
        let trash = json!({"action":"trash","id":REPORT});
        for answer in [
            json!({"action":"accept","content":{"decision":"deny"}}),
            json!({"action":"decline"}),
            json!({"action":"cancel"}),
        ] {
            let form = card(&mut f.server, trash.clone()).await;
            let reply = f
                .server
                .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":answer}))
                .await
                .unwrap();
            let value = text(&reply);
            assert_eq!(value["error"], "approval_denied", "{mode:?}: {value}");
            assert!(
                value["message"].as_str().unwrap().contains("nothing ran"),
                "{value}"
            );
            // A later approval of the same card runs nothing.
            assert!(f.server.handle(approve(&form)).await.is_none());
        }
        asks_on_card(&mut f.server, trash.clone()).await;
        // Nobody answers the change card, as in a scheduled job. Under reads_and_writes the
        // preview card is answered first, so the change card is the one that times out.
        let form = if mode == Some("reads_and_writes") {
            let preview = call(&mut f.server, trash.clone()).await;
            f.server.set_card_timeout(Duration::ZERO);
            f.server.handle(approve(&preview)).await.unwrap()
        } else {
            f.server.set_card_timeout(Duration::ZERO);
            call(&mut f.server, trash.clone()).await
        };
        assert_eq!(form["method"], "elicitation/create", "{form}");
        let message = form["params"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("Move to Recently Deleted "),
            "the change card times out: {message}"
        );
        let expired = f.server.expire_consent().unwrap();
        let value = text(&expired);
        assert_eq!(value["error"], "approval_timeout", "{mode:?}: {value}");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("Nobody answered"),
            "{value}"
        );
        assert!(f.server.handle(approve(&form)).await.is_none());
        assert!(writes(&f.remote).await.is_empty(), "{mode:?}");
    }
}

/// A card approved after the item changed in the cache runs nothing.
#[tokio::test]
async fn a_card_answered_after_the_item_changed_runs_nothing() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        serve_trash(&f.remote).await;
        let form = card(&mut f.server, json!({"action":"trash","id":REPORT})).await;
        change_cached_report(f.dir.path());
        let reply = f.server.handle(approve(&form)).await.unwrap();
        assert_eq!(reply["result"]["isError"], true, "{mode:?}: {reply}");
        assert!(writes(&f.remote).await.is_empty(), "{mode:?}");
    }
}

/// A folder without an approval mode, set to approve every change: every Drive change asks on
/// the card, even where a rule allows it, and the card offers no "Always allow".
#[tokio::test]
async fn require_approval_without_a_mode_asks_every_drive_change_on_the_card() {
    for rule in ["", "rules:\n  drive_write.create_folder: allow\n"] {
        let mut f = fixture(None).await;
        quince_core::Library::open(f.dir.path())
            .unwrap()
            .set_state("mcp.require_write_approval", "true")
            .unwrap();
        if !rule.is_empty() {
            rules(f.dir.path(), rule);
        }
        serve_listing(&f.remote, DOCS, docs_items(), None).await;
        answer(
            &f.remote,
            "/createFolders",
            json!({"folders":[{"drivewsid":"FOLDER::com.apple.CloudDocs::NEW","name":"Taxes","type":"FOLDER","status":"OK"}]}),
        )
        .await;
        let args = json!({"action":"create_folder","path":"/Documents","name":"Taxes"});
        let form = card(&mut f.server, args.clone()).await;
        assert!(writes(&f.remote).await.is_empty(), "{rule}");
        let done = text(&f.server.handle(approve(&form)).await.unwrap());
        assert_eq!(done["outcome"], "done", "{rule}: {done}");
        assert_eq!(writes(&f.remote).await, ["/createFolders"], "it ran once");
        assert!(f.server.handle(approve(&form)).await.is_none());
        assert_eq!(writes(&f.remote).await.len(), 1);
        // Without card support it is refused and nothing waits.
        let mut plain = Server::open(f.dir.path()).unwrap();
        init(&mut plain, false).await;
        let value = text(&call(&mut plain, args).await);
        assert_eq!(value["error"], "approval_requires_form_client", "{value}");
        assert!(value.get("request_id").is_none());
    }
}

/// The card's first line says the deletion in plain words.
#[tokio::test]
async fn a_drive_deletion_card_starts_with_a_plain_sentence() {
    let mut f = fixture(Some("yolo")).await;
    let form = card(
        &mut f.server,
        json!({"action":"delete_forever","id":REPORT}),
    )
    .await;
    let first = form["params"]["message"]
        .as_str()
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(first.starts_with("Delete forever \""), "{first}");
    assert!(
        first.ends_with(" in iCloud Drive. This can't be undone."),
        "{first}"
    );
}

/// The file's approval_mode is the mode in force for Drive too: yolo there runs a change
/// without a card where setup stored writes_only, and a trash still asks.
#[tokio::test]
async fn the_files_yolo_runs_a_drive_change_unasked_and_trash_still_asks() {
    let mut f = fixture(Some("writes_only")).await;
    rules(f.dir.path(), "version: 1\napproval_mode: yolo\n");
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
    let form = call(&mut f.server, json!({"action":"trash","id":REPORT})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
}
