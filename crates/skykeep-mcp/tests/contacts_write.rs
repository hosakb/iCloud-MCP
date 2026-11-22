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
    /// Apply the next write, then answer it with this status.
    store_then_fail: Option<u16>,
    /// The card changes on a phone just before the next write reaches it.
    edit_on_write: bool,
    /// Reads of the book so far, and from which read on they fail or are slow.
    startups: usize,
    fail_reads_from: Option<usize>,
    slow_reads_from: Option<usize>,
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
            book.startups += 1;
            if book.fail_reads_from.is_some_and(|n| book.startups >= n) {
                return ResponseTemplate::new(503);
            }
            let answer = ResponseTemplate::new(200).set_body_json(book.body());
            if book.slow_reads_from.is_some_and(|n| book.startups >= n) {
                return answer.set_delay(std::time::Duration::from_secs(5));
            }
            return answer;
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
        if std::mem::take(&mut book.edit_on_write) {
            if let Some(c) = book.cards.iter_mut().find(|c| c["contactId"] == id) {
                c["etag"] = json!("etag-raced");
            }
        }
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
        if let Some(status) = book.store_then_fail.take() {
            return ResponseTemplate::new(status);
        }
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

fn seam(uri: &str) -> Seam {
    let uri = uri.to_string();
    Seam {
        client: Some(Arc::new(move |dir: &Path| Ok(client(dir, &uri)))),
        ..Seam::default()
    }
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
    /// Reads of the book fail from the `n`th one after now (1 is the next).
    fn fail_reads_after(&self, n: usize) {
        let mut book = self.book.lock().unwrap();
        book.fail_reads_from = Some(book.startups + n);
    }
    fn slow_reads_after(&self, n: usize) {
        let mut book = self.book.lock().unwrap();
        book.slow_reads_from = Some(book.startups + n);
    }
    fn cards_at_apple(&self) -> usize {
        self.book.lock().unwrap().cards.len()
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
    server.set_contacts_seam(seam(&remote.uri()));
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
    assert_eq!(e["error"], "contact_nothing_to_change");
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
            "contact_field_too_long",
        ),
        (
            update_ada(json!({"notes":"n".repeat(4097)})),
            "contact_field_too_long",
        ),
        (
            update_ada(json!({"emails":many})),
            "contact_too_many_entries",
        ),
        (
            update_ada(json!({"emails":[{"value":"e".repeat(513)}]})),
            "contact_field_too_long",
        ),
        (
            update_ada(json!({"emails":[{"label":"l".repeat(65),"value":"a@b"}]})),
            "contact_label_too_long",
        ),
        (
            update_ada(json!({"emails":[{"value":" "}]})),
            "contact_empty_entry_value",
        ),
        (
            update_ada(json!({"birthday":"14/03/1990"})),
            "contact_invalid_birthday",
        ),
        (
            update_ada(json!({"first_name":"A\u{202E}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{7}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{E0041}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{FE0F}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{E0100}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{00AD}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{2028}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"first_name":"A\u{2029}da"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"notes":"line\u{2028}break"})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"emails":[{"value":"a\u{E0041}@example.com"}]})),
            "contact_text_not_allowed",
        ),
        (
            update_ada(json!({"birthday":"2001-02-31"})),
            "contact_invalid_birthday",
        ),
        (
            update_ada(json!({"photo":"data:"})),
            "contact_unknown_field",
        ),
        (
            update_ada(json!({"first_name":7})),
            "contact_field_must_be_text_or_null",
        ),
        (
            update_ada(json!({"emails":["ada@example.com"]})),
            "contact_entry_must_be_label_and_value",
        ),
        (
            update_ada(json!({"is_company":null})),
            "contact_is_company_must_be_true_or_false",
        ),
        (
            json!({"action":"update","guid":ADA,"fields":{"first_name":"X"}}),
            "contact_update_needs_guid_etag_and_fields",
        ),
        (
            json!({"action":"update","guid":ADA,"etag":"etag-ada-1"}),
            "contact_update_needs_guid_etag_and_fields",
        ),
        (
            json!({"action":"delete","guid":BOB}),
            "contact_delete_needs_only_guid_and_etag",
        ),
        (
            json!({"action":"delete","guid":BOB,"etag":"etag-bob-1","fields":{}}),
            "contact_delete_needs_only_guid_and_etag",
        ),
        (
            json!({"action":"delete","guid":"g".repeat(513),"etag":"e"}),
            "contact_invalid_guid_or_etag",
        ),
        (
            json!({"action":"create","guid":"C-CHOSEN","fields":{"first_name":"X"}}),
            "contact_create_needs_only_fields",
        ),
        (
            json!({"action":"create"}),
            "contact_create_needs_only_fields",
        ),
        (json!({"action":"merge"}), "contact_invalid_action"),
        (json!({"guid":BOB}), "contact_invalid_action"),
    ] {
        let reply = call(&mut f.server, args.clone()).await;
        assert_ne!(reply["method"], "elicitation/create", "{args}");
        let e = text(&reply);
        assert_eq!(e["error"], code, "{args}");
        // Every refusal here says what to do instead.
        {
            assert!(e["instructions"].is_string(), "{args}: {e}");
        }
        if code.starts_with("contact_update_") || code.starts_with("contact_delete_") {
            assert!(
                e["instructions"].as_str().unwrap().contains("contacts_get"),
                "{e}"
            );
        }
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
    let form = call(&mut f.server, delete_bob()).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let reply = f.server.handle(decide(&form, "approve")).await.unwrap();
    assert_eq!(text(&reply)["error"], "contact_changed_conflict", "{reply}");
    assert!(f.writes().is_empty());
    assert!(f.card_at_apple(BOB).is_some());
}

#[tokio::test]
async fn under_yolo_and_without_a_mode_a_delete_asks_on_the_card() {
    for policy in [Some("yolo"), None] {
        let mut f = fixture(policy).await;
        let form = call(&mut f.server, delete_bob()).await;
        assert_eq!(form["method"], "elicitation/create", "{policy:?}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"]);
        assert!(!form.to_string().contains("request_id"), "{form}");
        assert!(f.writes().is_empty());
        let reply = f.server.handle(decide(&form, "approve")).await.unwrap();
        let done = text(&reply);
        assert_eq!(done["outcome"], "done", "{policy:?}: {done}");
        assert!(f.card_at_apple(BOB).is_none());
        // Used once.
        assert!(f.server.handle(decide(&form, "approve")).await.is_none());
        assert_eq!(f.writes().len(), 1);
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
        shown.contains("because a deletion always needs a person"),
        "{shown}"
    );
    for mode in [Some("yolo"), None] {
        let mut f = fixture(mode).await;
        rules(f.dir.path(), "rules:\n  contacts_write.delete: allow\n");
        let form = call(&mut f.server, delete_bob()).await;
        assert_eq!(form["method"], "elicitation/create", "{mode:?}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"]);
        cancel(&mut f.server, &form).await;
        assert!(f.writes().is_empty());
    }
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

// --- Running at most once, and saying what is known.

#[tokio::test]
async fn a_terminal_approval_runs_once_even_when_apple_answers_with_an_error() {
    let mut f = fixture(Some("yolo")).await;
    rules(
        f.dir.path(),
        "rules:\n  contacts_write.create: {ask: terminal}\n",
    );
    let waiting = ok(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    let id = waiting["request_id"].as_str().unwrap().to_string();
    // Apple files the card, then the answer is lost in a server error.
    f.book.lock().unwrap().store_then_fail = Some(500);
    assert_eq!(
        f.server.approve_any(&id).await,
        Err("contacts_write_outcome_unknown_run_contacts_refresh")
    );
    assert_eq!(f.cards_at_apple(), 3);
    // The request is used up: approving it again makes no second card.
    assert!(f.server.approve_any(&id).await.is_err());
    assert_eq!(f.cards_at_apple(), 3);
    assert_eq!(f.writes().len(), 1);
}

#[tokio::test]
async fn a_card_changed_between_the_read_and_the_write_is_a_conflict_and_the_cache_is_kept() {
    let mut f = fixture(Some("yolo")).await;
    f.book.lock().unwrap().edit_on_write = true;
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(e["error"], "contact_changed_conflict", "{e}");
    assert_eq!(f.writes().len(), 1, "Apple refused the one write sent");
    let cached = f.cached(ADA).unwrap();
    assert_eq!(cached.contact.etag.as_deref(), Some("etag-ada-1"));
    assert_eq!(cached.contact.first_name.as_deref(), Some("Ada"));
}

#[tokio::test]
async fn an_unknown_outcome_leaves_the_cache_as_it_was() {
    let mut f = fixture(Some("yolo")).await;
    f.book.lock().unwrap().store_then_fail = Some(503);
    let e = error(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(
        e["error"], "contacts_write_outcome_unknown_run_contacts_refresh",
        "{e}"
    );
    let cached = f.cached(ADA).unwrap();
    assert_eq!(cached.contact.etag.as_deref(), Some("etag-ada-1"));
    assert_eq!(cached.contact.first_name.as_deref(), Some("Ada"));
}

#[tokio::test]
async fn a_failed_read_back_says_done_but_not_verified() {
    let mut f = fixture(Some("yolo")).await;
    // The read before the update works; the read after it fails.
    f.fail_reads_after(2);
    let done = ok(&mut f.server, update_ada(json!({"first_name":"Augusta"}))).await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], false, "{done}");
    assert_eq!(done["cache_updated"], false, "{done}");
    assert_eq!(f.card_at_apple(ADA).unwrap()["firstName"], "Augusta");
}

#[tokio::test]
async fn a_slow_read_back_says_done_but_not_verified() {
    let mut f = fixture(Some("yolo")).await;
    f.server.set_contacts_seam(Seam {
        check_timeout: std::time::Duration::from_secs(1),
        ..seam(&f.remote.uri())
    });
    f.slow_reads_after(2);
    let done = ok(&mut f.server, update_ada(json!({"nickname":"Countess"}))).await;
    assert_eq!(done["outcome"], "done", "{done}");
    assert_eq!(done["remote_verified"], false, "{done}");
    assert_eq!(f.card_at_apple(ADA).unwrap()["nickName"], "Countess");
}

#[tokio::test]
async fn a_failed_read_before_a_create_sends_nothing() {
    let mut f = fixture(Some("yolo")).await;
    f.fail_reads_after(1);
    let e = error(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    assert_eq!(e["error"], "contacts_write_read_failed_nothing_sent", "{e}");
    assert!(f.writes().is_empty());
}

// --- What the person sees before approving.

#[tokio::test]
async fn the_terminal_text_shows_each_change_with_its_current_value() {
    let mut f = fixture(Some("yolo")).await;
    rules(
        f.dir.path(),
        "rules:\n  contacts_write.update: {ask: terminal}\n  contacts_write.create: {ask: terminal}\n",
    );
    let waiting = ok(
        &mut f.server,
        update_ada(json!({"first_name":"Augusta","company":null,"phones":[{"label":"WORK","value":"+1 555 0199"}]})),
    )
    .await;
    let id = waiting["request_id"].as_str().unwrap().to_string();
    let shown = f.server.approval_text(&id).unwrap();
    for line in [
        "Set first name to Augusta (now: Ada)",
        "Clear company (now: Analytical Engines)",
        "Replace phones: remove MOBILE +1 555 0100; add WORK +1 555 0199",
    ] {
        assert!(shown.contains(line), "{line}\n{shown}");
    }
    assert!(!shown.contains('"'), "{shown}");
    // Notes are shown whole, up to their limit.
    let notes = "n".repeat(4000) + "END";
    let waiting = ok(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace","notes":notes}}),
    )
    .await;
    let id = waiting["request_id"].as_str().unwrap().to_string();
    let shown = f.server.approval_text(&id).unwrap();
    assert!(shown.contains(&notes), "{}", shown.len());
}

#[tokio::test]
async fn the_update_card_shows_what_is_replaced() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        update_ada(json!({"company":null,"phones":[{"value":"+1 555 0199"}]})),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("Analytical Engines"), "{message}");
    assert!(message.contains("+1 555 0100"), "{message}");
    cancel(&mut f.server, &form).await;
}

#[tokio::test]
async fn a_list_or_notes_contacts_get_cut_short_is_not_replaced() {
    let mut f = fixture(Some("writes_only")).await;
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut card = library.cards_by_guid(&[ADA.to_string()]).unwrap().remove(0);
        card.contact.notes = Some("n".repeat(600_000));
        for i in 0..150 {
            card.contact
                .methods
                .push(quince_core::contacts::ContactMethod {
                    kind: "email".into(),
                    label: None,
                    value: format!("a{i}@example.com"),
                });
        }
        library.put_card(&card).unwrap();
    }
    let got = text(
        &f.server
            .handle(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"contacts_get","arguments":{"guid":ADA}}}))
            .await
            .unwrap(),
    );
    assert_eq!(got["card_truncated"], true, "the fixture is cut short");
    for fields in [
        json!({"notes":"short"}),
        json!({"notes":null}),
        json!({"emails":[{"value":"x@example.com"}]}),
    ] {
        let reply = call(&mut f.server, update_ada(fields.clone())).await;
        assert_ne!(reply["method"], "elicitation/create", "{fields}");
        let e = text(&reply);
        assert_eq!(
            e["error"], "contact_shown_cut_short_edit_on_device",
            "{fields}"
        );
        assert!(e["instructions"].is_string());
    }
    // Fields contacts_get showed whole can still change.
    let form = call(
        &mut f.server,
        update_ada(json!({"phones":[{"value":"+1 555 0199"}]})),
    )
    .await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    cancel(&mut f.server, &form).await;
    assert!(f.writes().is_empty());
}

// --- Round two: what a refusal and an approval say.

#[tokio::test]
async fn a_refused_field_is_named_with_its_limit() {
    let mut f = fixture(Some("yolo")).await;
    for (fields, words) in [
        (json!({"first_name":"x".repeat(257)}), ["first name", "256"]),
        (json!({"notes":"n".repeat(4097)}), ["notes", "4096"]),
        (
            json!({"emails":(0..21).map(|i| json!({"value":format!("{i}@x")})).collect::<Vec<_>>()}),
            ["emails", "20"],
        ),
        (json!({"urls":[{"value":" "}]}), ["web addresses", "value"]),
        (json!({"job_title":"a\u{7}b"}), ["job title", "control"]),
        (
            json!({"emails":["ada@example.com"]}),
            ["emails", "\"label\""],
        ),
        (json!({}), ["Name at least one field", "fields"]),
    ] {
        let e = error(&mut f.server, update_ada(fields.clone())).await;
        let step = e["instructions"].as_str().unwrap_or_default();
        for word in words {
            assert!(step.contains(word), "{fields}: {word}: {e}");
        }
    }
    // A tool-neutral code from another tool gets no Contacts sentence.
    assert!(skykeep_mcp::next_step("invalid_action", f.dir.path(), &json!({})).is_none());
    assert!(skykeep_mcp::next_step("unknown_field", f.dir.path(), &json!({})).is_none());
}

#[tokio::test]
async fn a_delete_shows_what_it_takes_away() {
    let mut f = fixture(Some("yolo")).await;
    // The terminal text, for a folder that opted in with ask: terminal.
    rules(
        f.dir.path(),
        "rules:\n  contacts_write.delete: {ask: terminal}\n",
    );
    let waiting = ok(
        &mut f.server,
        json!({"action":"delete","guid":ADA,"etag":"etag-ada-1"}),
    )
    .await;
    let id = waiting["request_id"].as_str().unwrap().to_string();
    let shown = f.server.approval_text(&id).unwrap();
    for part in [
        "Contacts change: delete this card for good",
        "It deletes: name Ada Lovelace",
        "company Analytical Engines",
        "1 email",
        "1 phone",
        "1 postal address",
        "notes (32 characters)",
    ] {
        assert!(shown.contains(part), "{part}\n{shown}");
    }
    assert!(!shown.contains("a deletion"), "{shown}");
    // The card shows the same.
    let mut f = fixture(Some("writes_only")).await;
    let form = call(
        &mut f.server,
        json!({"action":"delete","guid":ADA,"etag":"etag-ada-1"}),
    )
    .await;
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("Analytical Engines"), "{message}");
    assert!(message.contains("postal"), "{message}");
    cancel(&mut f.server, &form).await;
}

#[tokio::test]
async fn a_terminal_request_that_cannot_run_is_refused_at_once() {
    for policy in [Some("yolo"), None] {
        let mut f = fixture(policy).await;
        for (args, code) in [
            (
                json!({"action":"delete","guid":BOB,"etag":"etag-old"}),
                "contact_changed_conflict",
            ),
            (
                json!({"action":"delete","guid":"C-NOBODY","etag":"e"}),
                "contact_not_in_cache_run_contacts_refresh",
            ),
        ] {
            let e = error(&mut f.server, args.clone()).await;
            assert_eq!(e["error"], code, "{policy:?} {args}: {e}");
        }
        rules(
            f.dir.path(),
            "rules:\n  contacts_write.update: {ask: terminal}\n",
        );
        let e = error(
            &mut f.server,
            json!({"action":"update","guid":ADA,"etag":"etag-old","fields":{"nickname":"A"}}),
        )
        .await;
        assert_eq!(e["error"], "contact_changed_conflict", "{policy:?}: {e}");
        assert!(f.writes().is_empty());
    }
}

#[tokio::test]
async fn reordered_or_repeated_entries_are_no_change() {
    let mut f = fixture(Some("yolo")).await;
    let done = ok(
        &mut f.server,
        update_ada(json!({"emails":[{"label":"HOME","value":"ada@example.com"},{"label":"WORK","value":"ada@work.example"}]})),
    )
    .await;
    assert_eq!(done["remote_verified"], true, "{done}");
    let etag = done["contact"]["etag"].as_str().unwrap().to_string();
    let again = ok(
        &mut f.server,
        json!({"action":"update","guid":ADA,"etag":etag,"fields":{"emails":[{"label":"WORK","value":"ada@work.example"},{"label":"HOME","value":"ada@example.com"}]}}),
    )
    .await;
    assert_eq!(again["outcome"], "unchanged", "{again}");
    assert_eq!(f.writes().len(), 1);
}

#[tokio::test]
async fn a_slow_read_before_the_write_sent_nothing() {
    let mut f = fixture(Some("yolo")).await;
    f.server.set_contacts_seam(Seam {
        read_timeout: std::time::Duration::from_secs(1),
        ..seam(&f.remote.uri())
    });
    f.slow_reads_after(1);
    let e = error(&mut f.server, update_ada(json!({"nickname":"A"}))).await;
    assert_eq!(e["error"], "contacts_write_read_failed_nothing_sent", "{e}");
    assert!(f.writes().is_empty());
}

#[test]
fn yolo_says_contacts_deletions_still_wait() {
    let yolo = skykeep_mcp::status::describe_mode("yolo");
    assert!(
        yolo.contains("Deletions") && yolo.contains("ask on the approval card"),
        "{yolo}"
    );
}

// --- Round three.

#[tokio::test]
async fn a_stale_terminal_request_says_it_is_out_of_date() {
    let mut f = fixture(Some("yolo")).await;
    rules(
        f.dir.path(),
        "rules:\n  contacts_write.delete: {ask: terminal}\n",
    );
    let waiting = ok(&mut f.server, delete_bob()).await;
    let id = waiting["request_id"].as_str().unwrap().to_string();
    {
        let mut library = quince_core::Library::open(f.dir.path()).unwrap();
        let mut card = library.cards_by_guid(&[BOB.to_string()]).unwrap().remove(0);
        card.contact.etag = Some("etag-bob-2".into());
        library.put_card(&card).unwrap();
    }
    assert_eq!(
        f.server.approval_text(&id),
        Err("contacts_write_request_out_of_date")
    );
    let step = skykeep_mcp::next_step(
        "contacts_write_request_out_of_date",
        f.dir.path(),
        &json!(null),
    )
    .unwrap();
    assert!(step.contains("out of date"), "{step}");
    assert!(step.contains("again"), "{step}");
    assert!(f.server.approve_any(&id).await.is_err());
    assert!(f.writes().is_empty());
}

#[tokio::test]
async fn the_card_preview_names_the_contact_once_and_drops_empty_fields() {
    let mut f = fixture(Some("writes_only")).await;
    let form = call(&mut f.server, delete_bob()).await;
    let message = form["params"]["message"].as_str().unwrap();
    assert_eq!(message.matches("\"contact\":").count(), 1, "{message}");
    assert!(!message.contains("\"company\": null"), "{message}");
    assert!(!message.contains("\"birthday\": null"), "{message}");
    assert!(message.contains("Bob Builder"), "{message}");
    cancel(&mut f.server, &form).await;
}

#[tokio::test]
async fn a_create_apple_refuses_for_a_stale_book_says_nothing_was_made() {
    let mut f = fixture(Some("yolo")).await;
    f.book.lock().unwrap().fail_with = Some(409);
    let e = error(
        &mut f.server,
        json!({"action":"create","fields":{"first_name":"Grace"}}),
    )
    .await;
    assert_eq!(e["error"], "contact_create_refused_try_again", "{e}");
    let step = e["instructions"].as_str().unwrap();
    assert!(step.contains("nothing was made"), "{step}");
    assert!(!step.contains("etag"), "{step}");
    assert_eq!(f.cards_at_apple(), 2);
}

// --- The approval card in every mode (issue 62).

const MODES: [Option<&str>; 4] = [
    Some("reads_and_writes"),
    Some("writes_only"),
    Some("yolo"),
    None,
];

/// The card that approves deleting Bob. Under reads_and_writes the preview read is approved
/// first. `timeout` applies to the change card.
async fn delete_card(f: &mut Fixture, mode: Option<&str>, timeout: std::time::Duration) -> Value {
    if mode == Some("reads_and_writes") {
        let preview = call(&mut f.server, delete_bob()).await;
        assert_eq!(preview["method"], "elicitation/create", "{preview}");
        f.server.set_card_timeout(timeout);
        return f.server.handle(decide(&preview, "approve")).await.unwrap();
    }
    f.server.set_card_timeout(timeout);
    call(&mut f.server, delete_bob()).await
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(180);

#[tokio::test]
async fn a_delete_asks_on_the_card_in_every_mode_and_runs_once() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        let form = delete_card(&mut f, mode, WAIT).await;
        assert_eq!(form["method"], "elicitation/create", "{mode:?}: {form}");
        assert_eq!(choices(&form), ["approve", "deny"], "{mode:?}");
        assert!(!form.to_string().contains("request_id"), "{mode:?}");
        assert!(f.writes().is_empty());
        let done = text(&f.server.handle(decide(&form, "approve")).await.unwrap());
        assert_eq!(done["outcome"], "done", "{mode:?}: {done}");
        assert!(f.card_at_apple(BOB).is_none());
        // A second use of the same answer is ignored.
        assert!(f.server.handle(decide(&form, "approve")).await.is_none());
        assert_eq!(f.writes().len(), 1, "{mode:?}");
    }
}

#[tokio::test]
async fn a_denied_cancelled_or_timed_out_delete_card_sends_nothing() {
    for mode in MODES {
        // Deny.
        let mut f = fixture(mode).await;
        let form = delete_card(&mut f, mode, WAIT).await;
        let e = text(&f.server.handle(decide(&form, "deny")).await.unwrap());
        assert_eq!(e["error"], "approval_denied", "{mode:?}: {e}");
        assert!(
            e["message"].as_str().unwrap().contains("nothing ran"),
            "{e}"
        );
        // Cancelled by the client.
        let form = delete_card(&mut f, mode, WAIT).await;
        assert_eq!(form["method"], "elicitation/create", "{mode:?}: {form}");
        let reply = f
            .server
            .handle(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}))
            .await
            .unwrap();
        assert_eq!(text(&reply)["error"], "approval_cancelled", "{reply}");
        assert!(f.server.handle(decide(&form, "approve")).await.is_none());
        // Nobody answers in time, as in a scheduled job.
        let form = delete_card(&mut f, mode, std::time::Duration::ZERO).await;
        assert_eq!(form["method"], "elicitation/create", "{mode:?}: {form}");
        let expired = text(&f.server.expire_consent().unwrap());
        assert_eq!(expired["error"], "approval_timeout", "{expired}");
        assert!(
            expired["message"]
                .as_str()
                .unwrap()
                .contains("scheduled job"),
            "{expired}"
        );
        assert!(f.server.handle(decide(&form, "approve")).await.is_none());
        assert!(f.writes().is_empty(), "{mode:?}");
        assert!(f.card_at_apple(BOB).is_some());
    }
}

#[tokio::test]
async fn a_card_for_a_card_changed_meanwhile_is_refused() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        let form = delete_card(&mut f, mode, WAIT).await;
        {
            let mut library = quince_core::Library::open(f.dir.path()).unwrap();
            let mut card = library.cards_by_guid(&[BOB.to_string()]).unwrap().remove(0);
            card.contact.last_name = Some("Changed".into());
            library.put_card(&card).unwrap();
        }
        let reply = f.server.handle(decide(&form, "approve")).await.unwrap();
        assert_eq!(reply["result"]["isError"], true, "{mode:?}: {reply}");
        assert!(f.writes().is_empty(), "{mode:?}");
    }
}

#[tokio::test]
async fn without_card_support_a_delete_is_refused_and_no_request_is_made() {
    for mode in MODES {
        let f = fixture(mode).await;
        let mut plain = Server::open(f.dir.path()).unwrap();
        plain.set_contacts_seam(seam(&f.remote.uri()));
        plain
            .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
            .await
            .unwrap();
        let e = error(&mut plain, delete_bob()).await;
        assert_eq!(e["error"], "approval_requires_form_client", "{mode:?}: {e}");
        let message = e["message"].as_str().unwrap();
        assert!(!message.contains("skykeep-mcp approve"), "{message}");
        assert!(message.contains("nothing ran"), "{message}");
        assert!(e.get("request_id").is_none(), "{e}");
        assert!(f.writes().is_empty());
    }
}

#[tokio::test]
async fn ask_terminal_stays_an_opt_in_that_still_works() {
    for mode in MODES {
        let mut f = fixture(mode).await;
        rules(
            f.dir.path(),
            "rules:\n  contacts_write.delete: {ask: terminal}\n",
        );
        let waiting = ok(&mut f.server, delete_bob()).await;
        assert_eq!(
            waiting["status"], "approval_required",
            "{mode:?}: {waiting}"
        );
        let id = waiting["request_id"].as_str().unwrap().to_string();
        assert!(f.writes().is_empty());
        let done = f.server.approve_any(&id).await.unwrap();
        assert_eq!(done["outcome"], "done", "{mode:?}: {done}");
        assert!(f.server.approve_any(&id).await.is_err());
        assert_eq!(f.writes().len(), 1);
    }
}
