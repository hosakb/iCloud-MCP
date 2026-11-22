//! Apple iCloud Contacts changes through `contacts_write`: create, update and delete against a
//! loopback fake of Apple's address book, the version check, the approval each needs and the
//! permissions file. Every fixture is synthetic; nothing reaches the network.
use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::{json, Value};
use skykeep_mcp::contacts_write::Seam;
use skykeep_mcp::Server;
use std::path::Path;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DSID: &str = "1234567890";
const ADA: &str = "C-ADA";
const BOB: &str = "C-BOB";

fn private_dir() -> tempfile::TempDir {
    unsafe {
        libc::umask(0o077);
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

// --- A fake of Apple's address book: one book, every write checked against the card's etag.

#[derive(Default)]
struct Book {
    cards: Vec<Value>,
    sync: u64,
    etags: u64,
    /// Every write as sent: (HTTP method, `method` query parameter, body).
    writes: Vec<(String, Option<String>, Value)>,
    /// Answer every write with this status instead.
    fail_with: Option<u16>,
}

impl Book {
    fn body(&self) -> Value {
        let order: Vec<Value> = self.cards.iter().map(|c| c["contactId"].clone()).collect();
        json!({
            "syncToken": format!("S-{}", self.sync),
            "prefToken": "PREF-1",
            "contactsOrder": order,
            "contacts": self.cards,
            "groups": [],
            "collections": [],
        })
    }
}

#[derive(Clone)]
struct Fake(Arc<Mutex<Book>>);

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut book = self.0.lock().unwrap();
        let path = request.url.path();
        if path == "/co/startup" {
            return ResponseTemplate::new(200).set_body_json(book.body());
        }
        if path.trim_end_matches('/') != "/co/contacts/card" {
            return ResponseTemplate::new(404);
        }
        let param = request
            .url
            .query_pairs()
            .find(|(k, _)| k == "method")
            .map(|(_, v)| v.to_string());
        let sent: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        book.writes
            .push((request.method.to_string(), param.clone(), sent.clone()));
        if let Some(status) = book.fail_with {
            return ResponseTemplate::new(status);
        }
        let card = sent["contacts"][0].clone();
        let id = card["contactId"].as_str().unwrap_or_default().to_string();
        let at = book.cards.iter().position(|c| c["contactId"] == id);
        let conflict =
            || ResponseTemplate::new(200).set_body_json(json!({"errors":[{"errorCode":409}]}));
        let stored = match (request.method.as_str(), param.as_deref()) {
            ("POST", Some("DELETE")) => {
                let Some(at) = at else { return conflict() };
                if book.cards[at]["etag"] != card["etag"] {
                    return conflict();
                }
                book.cards.remove(at);
                Value::Null
            }
            ("POST", None) => {
                book.etags += 1;
                let mut card = card;
                card["etag"] = json!(format!("etag-new-{}", book.etags));
                book.cards.push(card.clone());
                card
            }
            ("PUT", None) => {
                let Some(at) = at else { return conflict() };
                if book.cards[at]["etag"] != card["etag"] {
                    return conflict();
                }
                book.etags += 1;
                let mut card = card;
                card["etag"] = json!(format!("etag-new-{}", book.etags));
                book.cards[at] = card.clone();
                card
            }
            _ => return ResponseTemplate::new(400),
        };
        book.sync += 1;
        ResponseTemplate::new(200).set_body_json(json!({
            "syncToken": format!("S-{}", book.sync),
            "prefToken": "PREF-1",
            "contacts": if stored.is_null() { json!([]) } else { json!([stored]) },
        }))
    }
}

fn ada() -> Value {
    json!({
        "contactId": ADA,
        "firstName": "Ada",
        "lastName": "Lovelace",
        "companyName": "Analytical Engines",
        "notes": "Met at the synthetic conference.",
        "etag": "etag-ada-1",
        "emailAddresses": [{"label":"HOME","field":"ada@example.com"}],
        "phones": [{"label":"MOBILE","field":"+1 555 0100"}],
        "streetAddresses": [{"label":"HOME","field":{"street":"1 Synthetic Way","city":"London","country":"United Kingdom"}}],
        "someFieldTheCacheDoesNotKeep": "SYNTHETIC-UNKEPT",
    })
}

fn bob() -> Value {
    json!({"contactId": BOB, "firstName": "Bob", "lastName": "Builder", "etag": "etag-bob-1"})
}

fn client(dir: &Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"contacts":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    server: Server,
    remote: MockServer,
    book: Arc<Mutex<Book>>,
}

impl Fixture {
    fn writes(&self) -> Vec<(String, Option<String>, Value)> {
        self.book.lock().unwrap().writes.clone()
    }
    fn card_at_apple(&self, id: &str) -> Option<Value> {
        self.book
            .lock()
            .unwrap()
            .cards
            .iter()
            .find(|c| c["contactId"] == id)
            .cloned()
    }
    /// Someone edits the card on their phone: Apple's version moves on.
    fn edited_elsewhere(&self, id: &str, first: &str) {
        let mut book = self.book.lock().unwrap();
        let card = book
            .cards
            .iter_mut()
            .find(|c| c["contactId"] == id)
            .unwrap();
        card["firstName"] = json!(first);
        card["etag"] = json!("etag-phone-edit");
        book.sync += 1;
    }
    fn cached(&self, id: &str) -> Option<quince_core::contacts::Card> {
        let library = quince_core::Library::open(self.dir.path()).unwrap();
        library
            .cards_by_guid(&[id.to_string()])
            .unwrap()
            .into_iter()
            .next()
    }
}

/// A bound directory whose cache holds Ada and Bob as `contacts_refresh` reads them from the
/// fake, and the server pointed at it.
async fn fixture(policy: Option<&str>) -> Fixture {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"contacts":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let book = Arc::new(Mutex::new(Book {
        cards: vec![ada(), bob()],
        ..Book::default()
    }));
    let fake = Fake(book.clone());
    for p in ["/co/startup", "/co/contacts/card", "/co/contacts/card/"] {
        Mock::given(path(p))
            .respond_with(fake.clone())
            .mount(&remote)
            .await;
    }
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    {
        let mut library = quince_core::Library::open(dir.path()).unwrap();
        library
            .set_state("account", &quince_sync::queue::account_tag(DSID))
            .unwrap();
    }
    skykeep_mcp::contacts::refresh_with_client(dir.path(), client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    let uri = remote.uri();
    server.set_contacts_seam(Seam {
        client: Some(Arc::new(move |dir: &Path| Ok(client(dir, &uri)))),
        ..Seam::default()
    });
    init(&mut server).await;
    Fixture {
        dir,
        server,
        remote,
        book,
    }
}

async fn init(server: &mut Server) {
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}}))
        .await
        .unwrap();
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn call(server: &mut Server, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"contacts_write","arguments":args}}))
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

async fn cancel(server: &mut Server, form: &Value) {
    server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"cancel"}}))
        .await;
}

fn rules(dir: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(skykeep_mcp::permissions::FILE_NAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn update_ada(fields: Value) -> Value {
    json!({"action":"update","guid":ADA,"etag":"etag-ada-1","fields":fields})
}

fn delete_bob() -> Value {
    json!({"action":"delete","guid":BOB,"etag":"etag-bob-1"})
}

// --- The tool.

#[tokio::test]
async fn tools_list_has_one_contacts_write_tool_with_bounded_fields() {
    let mut f = fixture(None).await;
    let list = f
        .server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let tool = tools
        .iter()
        .find(|t| t["name"] == "contacts_write")
        .expect("contacts_write listed");
    let schema = &tool["inputSchema"];
    assert_eq!(
        schema["properties"]["action"]["enum"],
        json!(["create", "update", "delete"])
    );
    assert_eq!(schema["additionalProperties"], false);
    let fields = &schema["properties"]["fields"];
    assert_eq!(fields["additionalProperties"], false);
    assert_eq!(fields["properties"]["emails"]["maxItems"], 20);
    assert!(fields["properties"]["first_name"]["maxLength"].is_u64());
    assert!(fields["properties"].get("photo").is_none());
    assert_eq!(tool["annotations"]["destructiveHint"], true);
    // Every action is a permissions rule name; only delete removes, and for good.
    let names = skykeep_mcp::permissions::names();
    for action in ["create", "update", "delete"] {
        assert!(names.contains(&format!("contacts_write.{action}")));
    }
    assert_eq!(
        skykeep_mcp::permissions::removal_of("contacts_write", "delete"),
        skykeep_mcp::permissions::Removal::Permanent
    );
    assert_eq!(
        skykeep_mcp::permissions::removal_of("contacts_write", "update"),
        skykeep_mcp::permissions::Removal::None
    );
}

// --- Create.

#[tokio::test]
async fn create_sends_one_new_card_and_reads_it_back() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        json!({"action":"create","fields":{
            "first_name":"Grace","last_name":"Hopper","job_title":"Rear Admiral",
            "emails":[{"label":"WORK","value":"grace@example.com"}],
            "phones":[{"value":"+1 555 0123"}]
        }}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["action"], "create");
    assert_eq!(done["remote_verified"], true, "{done}");
    let guid = done["contact"]["guid"].as_str().unwrap().to_string();
    let etag = done["contact"]["etag"].as_str().unwrap();
    assert!(etag.starts_with("etag-new-"), "{done}");
    let writes = f.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let (verb, param, body) = &writes[0];
    assert_eq!((verb.as_str(), param.as_deref()), ("POST", None));
    let card = &body["contacts"][0];
    assert_eq!(card["contactId"], guid.as_str());
    assert_eq!(card["firstName"], "Grace");
    assert_eq!(card["jobTitle"], "Rear Admiral");
    assert_eq!(card["emailAddresses"][0]["field"], "grace@example.com");
    // The cache holds the card as Apple read it back, with Apple's version.
    let cached = f.cached(&guid).expect("cached");
    assert_eq!(cached.contact.etag.as_deref(), Some(etag));
    assert_eq!(cached.details.job_title.as_deref(), Some("Rear Admiral"));
}

#[tokio::test]
async fn a_blank_new_card_is_refused_before_anything_is_sent() {
    let mut f = fixture(Some("yolo")).await;
    for fields in [
        json!({}),
        json!({"notes":"only notes"}),
        json!({"first_name":"   "}),
    ] {
        let e = error(&mut f.server, json!({"action":"create","fields":fields})).await;
        assert_eq!(
            e["error"], "contact_needs_a_name_phone_or_email",
            "{fields}"
        );
    }
    assert!(f.writes().is_empty());
}

// --- Update.

#[tokio::test]
async fn update_changes_only_the_fields_given_and_keeps_the_rest_of_apples_card() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        update_ada(json!({"first_name":"Augusta","company":null,"phones":[{"label":"WORK","value":"+1 555 0199"}]})),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], true, "{done}");
    let writes = f.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let (verb, _, body) = &writes[0];
    assert_eq!(verb, "PUT");
    let sent = &body["contacts"][0];
    // Written against the version given.
    assert_eq!(sent["etag"], "etag-ada-1");
    assert_eq!(sent["firstName"], "Augusta");
    assert_eq!(sent["companyName"], "", "a null clears the field");
    assert_eq!(
        sent["phones"],
        json!([{"label":"WORK","field":"+1 555 0199"}])
    );
    // What was not named stays as Apple has it, fields the cache never kept included.
    assert_eq!(sent["lastName"], "Lovelace");
    assert_eq!(sent["notes"], "Met at the synthetic conference.");
    assert_eq!(sent["emailAddresses"][0]["field"], "ada@example.com");
    assert_eq!(sent["streetAddresses"][0]["field"]["city"], "London");
    assert_eq!(sent["someFieldTheCacheDoesNotKeep"], "SYNTHETIC-UNKEPT");
    // The reply and the cache carry Apple's new version, for the next update.
    let new_etag = done["contact"]["etag"].as_str().unwrap();
    assert_eq!(
        f.card_at_apple(ADA).unwrap()["etag"],
        json!(new_etag),
        "{done}"
    );
    let cached = f.cached(ADA).unwrap();
    assert_eq!(cached.contact.etag.as_deref(), Some(new_etag));
    assert_eq!(cached.contact.first_name.as_deref(), Some("Augusta"));
    assert_eq!(cached.contact.company, None);
    // A second update with the version from the first reply goes through too.
    let again = ok(
        &mut f.server,
        json!({"action":"update","guid":ADA,"etag":new_etag,"fields":{"nickname":"Ada"}}),
    )
    .await;
    assert_eq!(again["outcome"], "done", "{again}");
}

#[tokio::test]
async fn update_of_a_card_changed_elsewhere_is_refused_as_a_conflict_and_sends_nothing() {
    let mut f = fixture(Some("yolo")).await;
    f.edited_elsewhere(ADA, "Phone-Ada");
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(e["error"], "contact_changed_conflict", "{e}");
    assert!(e["instructions"]
        .as_str()
        .unwrap()
        .contains("contacts_refresh"));
    assert!(
        f.writes().is_empty(),
        "nothing is written over the phone's edit"
    );
    assert_eq!(f.card_at_apple(ADA).unwrap()["firstName"], "Phone-Ada");
}

#[tokio::test]
async fn a_version_that_is_not_the_cached_one_is_refused_before_any_request() {
    let mut f = fixture(Some("yolo")).await;
    let before = f.remote.received_requests().await.unwrap().len();
    let e = error(
        &mut f.server,
        json!({"action":"update","guid":ADA,"etag":"etag-made-up","fields":{"first_name":"X"}}),
    )
    .await;
    assert_eq!(e["error"], "contact_changed_conflict", "{e}");
    let e = error(
        &mut f.server,
        json!({"action":"update","guid":"C-NOBODY","etag":"e","fields":{"first_name":"X"}}),
    )
    .await;
    assert_eq!(e["error"], "contact_not_in_cache_run_contacts_refresh");
    assert_eq!(f.remote.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn apple_refusing_the_version_is_reported_as_a_conflict() {
    let mut f = fixture(Some("yolo")).await;
    f.book.lock().unwrap().fail_with = Some(412);
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(e["error"], "contact_changed_conflict", "{e}");
    f.book.lock().unwrap().fail_with = Some(503);
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(
        e["error"], "contacts_write_outcome_unknown_run_contacts_refresh",
        "{e}"
    );
}

#[tokio::test]
async fn an_update_apple_already_holds_sends_nothing() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        update_ada(
            json!({"first_name":"Ada","emails":[{"label":"HOME","value":"ada@example.com"}]}),
        ),
    )
    .await;
    assert_eq!(done["outcome"], "unchanged", "{done}");
    assert!(f.writes().is_empty());
    let e = error(&mut f.server, update_ada(json!({}))).await;
    assert_eq!(e["error"], "nothing_to_change");
}

// --- Bounds.

#[tokio::test]
async fn oversized_or_malformed_arguments_are_refused_before_approval_or_network() {
    let mut f = fixture(Some("writes_only")).await;
    let before = f.remote.received_requests().await.unwrap().len();
    let many: Vec<Value> = (0..21)
        .map(|i| json!({"value":format!("p{i}@example.com")}))
        .collect();
    for (args, code) in [
        (
            update_ada(json!({"first_name":"x".repeat(257)})),
            "field_too_long",
        ),
        (
            update_ada(json!({"notes":"n".repeat(4097)})),
            "field_too_long",
        ),
        (update_ada(json!({"emails":many})), "too_many_entries"),
        (
            update_ada(json!({"emails":[{"value":"e".repeat(513)}]})),
            "field_too_long",
        ),
        (
            update_ada(json!({"emails":[{"label":"l".repeat(65),"value":"a@b"}]})),
            "field_too_long",
        ),
        (
            update_ada(json!({"emails":[{"value":" "}]})),
            "empty_entry_value",
        ),
        (
            update_ada(json!({"birthday":"14/03/1990"})),
            "invalid_birthday",
        ),
        (
            update_ada(json!({"first_name":"A\u{202E}da"})),
            "invalid_text",
        ),
        (update_ada(json!({"first_name":"A\u{7}da"})), "invalid_text"),
        (update_ada(json!({"photo":"data:"})), "invalid_arguments"),
        (
            json!({"action":"update","guid":ADA,"fields":{"first_name":"X"}}),
            "invalid_arguments",
        ),
        (json!({"action":"delete","guid":BOB}), "invalid_arguments"),
        (
            json!({"action":"delete","guid":"g".repeat(513),"etag":"e"}),
            "invalid_arguments",
        ),
        (
            json!({"action":"create","guid":"C-CHOSEN","fields":{"first_name":"X"}}),
            "invalid_arguments",
        ),
        (json!({"action":"merge"}), "invalid_arguments"),
    ] {
        let reply = call(&mut f.server, args.clone()).await;
        assert_ne!(reply["method"], "elicitation/create", "{args}");
        assert_eq!(text(&reply)["error"], code, "{args}");
    }
    assert_eq!(f.remote.received_requests().await.unwrap().len(), before);
}

// --- Delete.

#[tokio::test]
async fn delete_always_asks_on_a_card_without_always_allow_and_is_verified() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(&mut f.server, delete_bob()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny"]);
    let message = form["params"]["message"].as_str().unwrap();
    assert!(
        message.contains("Bob"),
        "the card names the contact: {message}"
    );
    let reply = f.server.handle(decide(&form, "approve")).await.unwrap();
    let done = text(&reply);
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], true, "{done}");
    let writes = f.writes();
    assert_eq!(writes.len(), 1);
    let (verb, param, body) = &writes[0];
    assert_eq!((verb.as_str(), param.as_deref()), ("POST", Some("DELETE")));
    assert_eq!(body["contacts"][0]["etag"], "etag-bob-1");
    assert!(f.card_at_apple(BOB).is_none());
    assert!(f.cached(BOB).is_none(), "the cache forgets the card");
}

#[tokio::test]
async fn delete_of_a_card_changed_elsewhere_is_refused() {
    let mut f = fixture(Some("yolo")).await;
    f.edited_elsewhere(BOB, "Robert");
    let id = ok(&mut f.server, delete_bob()).await["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("contact_changed_conflict")
    );
    assert!(f.writes().is_empty());
    assert!(f.card_at_apple(BOB).is_some());
}

#[tokio::test]
async fn under_yolo_and_without_a_mode_a_delete_waits_for_the_terminal() {
    for policy in [Some("yolo"), None] {
        let mut f = fixture(policy).await;
        let waiting = ok(&mut f.server, delete_bob()).await;
        assert_eq!(
            waiting["status"], "approval_required",
            "{policy:?}: {waiting}"
        );
        assert!(f.writes().is_empty());
        let id = waiting["request_id"].as_str().unwrap().to_string();
        let shown = f.server.approval_text(&id).unwrap();
        assert!(shown.contains("Bob Builder"), "{shown}");
        let done = f.server.approve_any(&id).await.unwrap();
        assert_eq!(done["outcome"], "done", "{policy:?}: {done}");
        assert!(f.card_at_apple(BOB).is_none());
        // Used once.
        assert!(f.server.approve_any(&id).await.is_err());
    }
}

// --- Approval modes and the permissions file.

#[tokio::test]
async fn writes_only_asks_for_create_and_update_and_always_allow_saves_a_rule() {
    let mut f = fixture(Some("writes_only")).await;
    let args = update_ada(json!({"first_name":"Augusta"}));
    let form = call(&mut f.server, args.clone()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny", "always_allow"]);
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("etag-ada-1"), "{message}");
    // Deny sends nothing.
    let denied = f.server.handle(decide(&form, "deny")).await.unwrap();
    assert_eq!(text(&denied)["error"], "approval_denied");
    assert!(f.writes().is_empty());
    let form = call(&mut f.server, args).await;
    let reply = f
        .server
        .handle(decide(&form, "always_allow"))
        .await
        .unwrap();
    let done = text(&reply);
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["permission_rule"]["saved"], true, "{done}");
    let saved =
        std::fs::read_to_string(f.dir.path().join(skykeep_mcp::permissions::FILE_NAME)).unwrap();
    assert!(saved.contains("contacts_write.update: allow"), "{saved}");
    // The next update runs without a card; a create still asks.
    let etag = done["contact"]["etag"].as_str().unwrap().to_string();
    let next = ok(
        &mut f.server,
        json!({"action":"update","guid":ADA,"etag":etag,"fields":{"nickname":"Ada"}}),
    )
    .await;
    assert_eq!(next["outcome"], "done", "{next}");
    let form = call(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create");
    cancel(&mut f.server, &form).await;
}

#[tokio::test]
async fn an_allow_rule_never_lets_a_delete_run_unasked() {
    let mut f = fixture(Some("writes_only")).await;
    rules(
        f.dir.path(),
        "rules:\n  contacts_write: allow\n  contacts_write.delete: allow\n",
    );
    let form = call(&mut f.server, delete_bob()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(choices(&form), ["approve", "deny"]);
    cancel(&mut f.server, &form).await;
    let shown = skykeep_mcp::permissions::show(f.dir.path(), false).unwrap();
    assert!(
        shown.contains("a Contacts deletion always needs a person"),
        "{shown}"
    );
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  contacts_write.delete: allow\n");
    let waiting = ok(&mut f.server, delete_bob()).await;
    assert_eq!(waiting["status"], "approval_required", "{waiting}");
    assert!(f.writes().is_empty());
}

#[tokio::test]
async fn a_deny_rule_refuses_and_a_broken_file_keeps_it() {
    let mut f = fixture(Some("yolo")).await;
    rules(f.dir.path(), "rules:\n  contacts_write.update: deny\n");
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(e["error"], "denied_by_permissions");
    assert_eq!(e["rule"], "contacts_write.update");
    // An unusable file never loosens: the last valid deny stays.
    rules(f.dir.path(), "rules:\n  contacts_write.update: maybe\n");
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(e["error"], "denied_by_permissions");
    // Create was never denied and still runs under yolo.
    let done = ok(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(f.writes().len(), 1);
}

#[tokio::test]
async fn a_broken_file_with_no_earlier_rules_makes_a_delete_ask_and_a_terminal_rule_waits() {
    let mut f = fixture(Some("writes_only")).await;
    rules(f.dir.path(), "rules:\n  nonsense\n");
    let form = call(&mut f.server, delete_bob()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    cancel(&mut f.server, &form).await;
    rules(
        f.dir.path(),
        "rules:\n  contacts_write.create: {ask: terminal}\n",
    );
    let waiting = ok(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    assert_eq!(waiting["status"], "approval_required", "{waiting}");
    let id = waiting["request_id"].as_str().unwrap().to_string();
    let done = f.server.approve_any(&id).await.unwrap();
    assert_eq!(done["outcome"], "done", "{done}");
}

#[tokio::test]
async fn reads_and_writes_shows_the_cached_card_before_the_change_card() {
    let mut f = fixture(Some("reads_and_writes")).await;
    let args = update_ada(json!({"first_name":"Augusta"}));
    let first = call(&mut f.server, args).await;
    assert_eq!(first["method"], "elicitation/create", "{first}");
    // The first card asks only to read the cached card; it does not show it yet.
    assert!(!first["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Lovelace"));
    let second = f.server.handle(decide(&first, "approve")).await.unwrap();
    assert_eq!(second["method"], "elicitation/create", "{second}");
    assert!(second["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Lovelace"));
    // The cached card changes between the card and the answer: the approval is stale.
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut card = library.cards_by_guid(&[ADA.to_string()]).unwrap().remove(0);
        card.contact.last_name = Some("Byron".into());
        library.put_card(&card).unwrap();
    }
    let reply = f.server.handle(decide(&second, "approve")).await.unwrap();
    assert_eq!(text(&reply)["error"], "approval_policy_changed", "{reply}");
    assert!(f.writes().is_empty());
}
