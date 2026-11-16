//! Apple iCloud Contacts, read-only: cached list, search and get, and an explicit refresh.
//! Every fixture is synthetic and every remote is a loopback mock.
use quince_auth::{session::Session, Client, Endpoints, Store};
use quince_core::contacts::{Contact, ContactDetails, ContactMethod, PostalAddress};
use quince_icloud::contacts::{AddressBook, ContactList};
use serde_json::{json, Value};
use skykeep_mcp::Server;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DSID: &str = "1234567890";

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn method_of(kind: &str, label: Option<&str>, value: &str) -> ContactMethod {
    ContactMethod {
        kind: kind.into(),
        label: label.map(Into::into),
        value: value.into(),
    }
}

fn person(guid: &str, first: Option<&str>, last: Option<&str>) -> Contact {
    Contact {
        guid: guid.into(),
        first_name: first.map(Into::into),
        middle_name: None,
        photo_url: None,
        photo_signature: None,
        photo_crop: None,
        last_name: last.map(Into::into),
        company: None,
        is_company: false,
        birthday: None,
        notes: None,
        etag: Some(format!("etag-{guid}")),
        methods: Vec::new(),
    }
}

/// A synthetic book: Unicode names, a tie on name, a company card, a phone-only card, and one
/// person with several emails and phones, a photo, a postal address, details and a list.
fn synthetic_book() -> AddressBook {
    let mut ada = person("C-ADA", Some("Ada"), Some("Lovelace"));
    ada.middle_name = Some("King".into());
    ada.birthday = Some("1815-12-10".into());
    ada.notes = Some("Synthetic note".into());
    ada.photo_url = Some("https://p01-contacts.icloud.com/SYNTHETIC-PHOTO-URL".into());
    ada.photo_signature = Some("SYNTHETIC-SIGNATURE".into());
    ada.methods = vec![
        method_of("phone", Some("MOBILE"), "+44 20 7946 0001"),
        method_of("phone", Some("_$!<Work>!$_"), "+44 20 7946 0002"),
        method_of("email", Some("HOME"), "ada@example.com"),
        method_of("email", Some("Analytical"), "ada@engine.example"),
        method_of("email", None, "countess@example.org"),
        method_of("address", Some("HOME"), "12 Synthetic Square\nLondon"),
        method_of("related", Some("MOTHER"), "Anne"),
        method_of("im", Some("SKYPE"), "ada.l"),
    ];
    let mut zoe = person("C-ZOE", Some("Zoë"), Some("Ångström"));
    zoe.methods = vec![method_of("phone", Some("MOBILE"), "+91 98765-43210")];
    let emile = person("C-EMILE", Some("Émile"), Some("Zola"));
    let sam_b = person("C-SAM-B", Some("Sam"), Some("Lee"));
    let sam_a = person("C-SAM-A", Some("Sam"), Some("Lee"));
    let mut acme = person("C-ACME", None, None);
    acme.company = Some("Acme 東京".into());
    acme.is_company = true;
    let mut number = person("C-NUMBER", None, None);
    number.methods = vec![method_of("phone", None, "+1 555 0100")];
    AddressBook {
        // Stored order deliberately differs from display order.
        contacts: vec![sam_b, zoe, number, acme, emile, ada, sam_a],
        lists: vec![ContactList {
            guid: "L-FRIENDS".into(),
            name: "Friends ✨".into(),
            members: vec!["C-ADA".into(), "C-ZOE".into()],
            etag: Some("list-etag".into()),
        }],
        me: None,
        addresses: vec![(
            "C-ADA".into(),
            vec![PostalAddress {
                label: Some("HOME".into()),
                street: Some("12 Synthetic Square".into()),
                city: Some("London".into()),
                country_code: Some("gb".into()),
                ..PostalAddress::default()
            }],
        )],
        details: vec![(
            "C-ADA".into(),
            ContactDetails {
                nickname: Some("Countess".into()),
                job_title: Some("Analyst".into()),
                ..ContactDetails::default()
            },
        )],
    }
}

/// A private directory bound to the synthetic account, holding the synthetic book.
fn seeded(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    quince_icloud::contacts::apply(&mut library, synthetic_book()).unwrap();
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

const DISPLAY_ORDER: [&str; 7] = [
    // Folded sort on last name then first, ties by GUID; a company and a
    // phone-only card sort by what they show.
    "C-NUMBER", "C-ACME", "C-ZOE", "C-SAM-A", "C-SAM-B", "C-ADA", "C-EMILE",
];

#[tokio::test]
async fn tools_list_names_apple_icloud_contacts_with_strict_read_only_schemas() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let init = server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Apple iCloud Contacts"),
        "{instructions}"
    );
    assert!(instructions.contains("read-only"), "{instructions}");
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let contacts: Vec<&Value> = tools
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with("contacts"))
        .collect();
    let names: Vec<&str> = contacts
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "contacts_list",
            "contacts_search",
            "contacts_get",
            "contacts_refresh"
        ]
    );
    for tool in &contacts {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("Apple iCloud Contacts"),
            "{name}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        let hints = &tool["annotations"];
        assert_eq!(hints["destructiveHint"], false, "{name}");
        if name == "contacts_refresh" {
            assert_eq!(
                hints["readOnlyHint"], false,
                "refresh replaces the local cache"
            );
            assert_eq!(hints["openWorldHint"], true, "refresh reads from Apple");
            assert_eq!(tool["inputSchema"]["properties"], json!({}));
        } else {
            assert_eq!(hints["readOnlyHint"], true, "{name}");
            assert_eq!(hints["openWorldHint"], false, "{name}");
        }
    }
    let search = contacts[1];
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert_eq!(
        search["inputSchema"]["properties"]["query"]["maxLength"],
        256
    );
    assert_eq!(search["inputSchema"]["properties"]["limit"]["maximum"], 100);
    assert_eq!(contacts[2]["inputSchema"]["required"], json!(["guid"]));
    // This phase reads only: nothing writes, deletes, merges, imports, exports or downloads.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for word in ["photo", "vcard", "import", "export", "merge", "csv"] {
            assert!(!name.contains(word), "{name}");
        }
        if name.starts_with("contacts") {
            assert!(
                !name.contains("write") && !name.contains("delete"),
                "{name}"
            );
        }
    }
}

#[tokio::test]
async fn list_pages_in_a_stable_order_with_bounded_limits() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = ok(
            &mut server,
            "contacts_list",
            json!({"offset":offset,"limit":3}),
        )
        .await;
        assert_eq!(page["source"], "local_cache");
        assert_eq!(page["total"], 7);
        assert_eq!(page["complete_representation"], false);
        assert!(page["items"].as_array().unwrap().len() <= 3);
        seen.extend(guids(&page));
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(seen, DISPLAY_ORDER);
    // The same request answers the same way.
    let a = ok(&mut server, "contacts_list", json!({})).await;
    let b = ok(&mut server, "contacts_list", json!({})).await;
    assert_eq!(a, b);
    assert_eq!(a["limit"], 25, "default page size");
    let past = ok(&mut server, "contacts_list", json!({"offset":99})).await;
    assert_eq!(past["items"], json!([]));
    assert!(past["next_offset"].is_null());
    for bad in [
        json!({"limit":0}),
        json!({"limit":101}),
        json!({"offset":-1}),
        json!({"limit":"5"}),
        json!({"parent":"L-FRIENDS"}),
        json!({"include_photos":true}),
    ] {
        assert_eq!(
            error(&mut server, "contacts_list", bad.clone()).await,
            "invalid_arguments",
            "{bad}"
        );
    }
}

#[tokio::test]
async fn summaries_carry_every_email_and_phone_and_mark_what_is_left_out() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let page = ok(&mut server, "contacts_list", json!({"limit":100})).await;
    let ada = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["guid"] == "C-ADA")
        .unwrap()
        .clone();
    assert_eq!(ada["display_name"], "Ada King Lovelace");
    assert_eq!(ada["emails"].as_array().unwrap().len(), 3);
    assert_eq!(ada["phones"].as_array().unwrap().len(), 2);
    assert_eq!(
        ada["emails"][1],
        json!({"label":"Analytical","value":"ada@engine.example"})
    );
    assert_eq!(ada["has_photo"], true);
    let zoe = &page["items"][2];
    assert_eq!(zoe["display_name"], "Zoë Ångström");
    assert_eq!(page["items"][1]["display_name"], "Acme 東京");
    assert_eq!(page["items"][1]["is_company"], true);
    assert_eq!(page["items"][0]["display_name"], "+1 555 0100");
    let raw = page.to_string();
    assert!(
        !raw.contains("SYNTHETIC-PHOTO-URL"),
        "photo locations stay local"
    );
    assert!(!raw.contains("SYNTHETIC-SIGNATURE"));
}

#[tokio::test]
async fn search_matches_names_emails_and_numbers_across_scripts() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    for (query, expected) in [
        ("zoe", vec!["C-ZOE"]),
        ("ÅNGSTRÖM", vec!["C-ZOE"]),
        ("98765 43210", vec!["C-ZOE"]),
        ("engine.example", vec!["C-ADA"]),
        ("countess", vec!["C-ADA"]),
        ("東京", vec!["C-ACME"]),
        ("sam lee", vec!["C-SAM-A", "C-SAM-B"]),
        ("nobody-matches-this", vec![]),
    ] {
        let page = ok(&mut server, "contacts_search", json!({"query":query})).await;
        assert_eq!(guids(&page), expected, "{query}");
        assert_eq!(page["total"], expected.len(), "{query}");
        assert_eq!(page["source"], "local_cache");
    }
    let page = ok(
        &mut server,
        "contacts_search",
        json!({"query":"sam","limit":1}),
    )
    .await;
    assert_eq!(guids(&page), ["C-SAM-A"]);
    assert_eq!(page["next_offset"], 1);
    let page = ok(
        &mut server,
        "contacts_search",
        json!({"query":"sam","offset":1,"limit":1}),
    )
    .await;
    assert_eq!(guids(&page), ["C-SAM-B"]);
    assert!(page["next_offset"].is_null());
    assert_eq!(
        error(&mut server, "contacts_search", json!({"query":"   "})).await,
        "query_required"
    );
    assert_eq!(
        error(
            &mut server,
            "contacts_search",
            json!({"query":"x".repeat(257)})
        )
        .await,
        "query_too_long"
    );
    assert_eq!(
        error(&mut server, "contacts_search", json!({})).await,
        "invalid_arguments"
    );
}

#[tokio::test]
async fn get_returns_the_cached_card_and_says_what_is_not_kept() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let got = ok(&mut server, "contacts_get", json!({"guid":"C-ADA"})).await;
    assert_eq!(got["source"], "local_cache");
    assert_eq!(got["complete_representation"], false);
    let card = &got["contact"];
    assert_eq!(card["guid"], "C-ADA");
    assert_eq!(card["first_name"], "Ada");
    assert_eq!(card["middle_name"], "King");
    assert_eq!(card["birthday"], "1815-12-10");
    assert_eq!(card["notes"], "Synthetic note");
    assert_eq!(card["etag"], "etag-C-ADA");
    assert_eq!(card["details"]["nickname"], "Countess");
    assert_eq!(card["details"]["job_title"], "Analyst");
    assert_eq!(card["emails"].as_array().unwrap().len(), 3);
    assert_eq!(
        card["phones"][1],
        json!({"label":"_$!<Work>!$_","value":"+44 20 7946 0002"})
    );
    assert_eq!(card["postal_addresses"][0]["city"], "London");
    assert_eq!(card["postal_addresses"][0]["street"], "12 Synthetic Square");
    let others = card["other_methods"].as_array().unwrap();
    assert!(others.contains(&json!({"kind":"related","label":"MOTHER","value":"Anne"})));
    assert!(others.contains(&json!({"kind":"im","label":"SKYPE","value":"ada.l"})));
    assert!(
        others.iter().all(|m| m["kind"] != "address"),
        "addresses are structured"
    );
    assert_eq!(
        card["lists"],
        json!([{"guid":"L-FRIENDS","name":"Friends ✨"}])
    );
    assert_eq!(
        card["photo"],
        json!({"present":true,"image_included":false})
    );
    let not_included = got["not_included"].as_array().unwrap();
    for needle in ["photo", "vCard", "custom"] {
        assert!(
            not_included
                .iter()
                .any(|n| n.as_str().unwrap().contains(needle)),
            "{needle}"
        );
    }
    let raw = got.to_string();
    assert!(!raw.contains("SYNTHETIC-PHOTO-URL") && !raw.contains("SYNTHETIC-SIGNATURE"));
    let plain = ok(&mut server, "contacts_get", json!({"guid":"C-EMILE"})).await;
    assert_eq!(plain["contact"]["lists"], json!([]));
    assert_eq!(plain["contact"]["photo"]["present"], false);
    assert_eq!(
        error(&mut server, "contacts_get", json!({"guid":"C-MISSING"})).await,
        "not_found"
    );
    assert_eq!(
        error(&mut server, "contacts_get", json!({"guid":""})).await,
        "invalid_arguments"
    );
    assert_eq!(
        error(&mut server, "contacts_get", json!({"guid":"g".repeat(513)})).await,
        "invalid_arguments"
    );
    assert_eq!(
        error(
            &mut server,
            "contacts_get",
            json!({"guid":"C-ADA","vcard":true})
        )
        .await,
        "invalid_arguments"
    );
}

#[tokio::test]
async fn reads_say_when_the_cache_was_never_refreshed_or_last_failed() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    let empty = ok(&mut server, "contacts_list", json!({})).await;
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["cache"]["refreshed"], false);
    assert!(empty["cache"]["last_refreshed_at"].is_null());
    assert_eq!(empty["remote_verified"], false);
    let (dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let fresh = ok(&mut server, "contacts_list", json!({})).await;
    assert_eq!(fresh["cache"]["refreshed"], true);
    assert!(fresh["cache"]["last_refreshed_at"].is_i64());
    assert_eq!(fresh["cache"]["last_refresh_failed"], false);
    quince_core::Library::open(dir.path())
        .unwrap()
        .mark_sync_failed("contacts", "SYNTHETIC-HTTP-BODY")
        .unwrap();
    let failed = ok(&mut server, "contacts_search", json!({"query":"ada"})).await;
    assert_eq!(failed["cache"]["last_refresh_failed"], true);
    assert!(!failed.to_string().contains("SYNTHETIC-HTTP-BODY"));
}

#[tokio::test]
async fn writes_only_and_yolo_read_contacts_without_a_prompt() {
    for mode in ["writes_only", "yolo"] {
        let (_dir, mut server) = seeded(Some(mode));
        init(&mut server, true).await;
        for (name, args) in [
            ("contacts_list", json!({})),
            ("contacts_search", json!({"query":"ada"})),
            ("contacts_get", json!({"guid":"C-ADA"})),
        ] {
            let reply = call(&mut server, 3, name, args).await;
            assert!(reply.get("method").is_none(), "{mode} {name}: {reply}");
            assert_eq!(reply["result"]["isError"], false, "{mode} {name}: {reply}");
        }
    }
}

#[tokio::test]
async fn reads_and_writes_binds_each_contacts_read_to_one_approval() {
    let (dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(&mut server, 3, "contacts_get", json!({"guid":"C-ADA"})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    // The preview escapes markup characters such as `_` as inert JSON text.
    assert!(
        message.contains("contacts\\u005fget") && message.contains("C-ADA"),
        "{message}"
    );
    assert!(!message.contains("Lovelace"), "no content before approval");
    // Nothing else runs while it waits, and a stray answer changes nothing.
    assert_eq!(
        text(&call(&mut server, 4, "contacts_list", json!({})).await)["error"],
        "approval_pending"
    );
    assert!(server
        .handle(json!({"jsonrpc":"2.0","id":"other","result":{"action":"accept","content":{}}}))
        .await
        .is_none());
    let accepted = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(accepted["id"], 3);
    assert_eq!(text(&accepted)["contact"]["last_name"], "Lovelace");
    // A replayed answer is not a second approval.
    assert!(server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .is_none());

    // Declined, or answered with content, is a denial.
    for answer in [
        json!({"action":"decline"}),
        json!({"action":"accept","content":{"approve":true}}),
    ] {
        let form = call(&mut server, 5, "contacts_list", json!({})).await;
        let reply = server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":answer}))
            .await
            .unwrap();
        assert_eq!(reply["id"], 5);
        assert_eq!(text(&reply)["error"], "approval_denied");
    }
    // Cancelled by the client.
    let form = call(&mut server, 6, "contacts_search", json!({"query":"ada"})).await;
    let reply = server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":6}}),
        )
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_cancelled");
    assert!(server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .is_none());
    // A policy change between the prompt and the answer makes the approval stale.
    let form = call(&mut server, 8, "contacts_get", json!({"guid":"C-ADA"})).await;
    Server::open(dir.path())
        .unwrap()
        .set_policy("reads_and_writes")
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_policy_changed");
    // The tool arguments cannot carry their own approval.
    assert_eq!(
        text(
            &call(
                &mut server,
                9,
                "contacts_get",
                json!({"guid":"C-ADA","approved":true})
            )
            .await
        )["error"],
        "invalid_arguments"
    );
}

#[tokio::test]
async fn reads_and_writes_without_form_support_refuses_contacts_reads() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, false).await;
    for (name, args) in [
        ("contacts_list", json!({})),
        ("contacts_get", json!({"guid":"C-ADA"})),
        ("contacts_refresh", json!({})),
    ] {
        assert_eq!(
            error(&mut server, name, args).await,
            "approval_requires_form_client"
        );
    }
}

#[tokio::test]
async fn refresh_prompt_names_contacts_only_and_sends_nothing() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(&mut server, 3, "contacts_refresh", json!({})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("contacts\\u005frefresh"), "{message}");
    assert!(
        message.contains("\"services\": \\u005b\n      \"contacts\"\n    \\u005d"),
        "{message}"
    );
    assert!(
        !message.contains("notes") && !message.contains("reminders"),
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
async fn refresh_needs_a_bound_account_before_any_keyring_or_network_use() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    assert_eq!(
        error(&mut server, "contacts_refresh", json!({})).await,
        "login_required"
    );
    assert_eq!(
        error(&mut server, "contacts_refresh", json!({"full":true})).await,
        "invalid_arguments"
    );
}

// --- Refresh against a synthetic Apple.

fn startup_body(contacts: Value, sync: &str) -> Value {
    let order: Vec<Value> = contacts
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["contactId"].clone())
        .collect();
    json!({
        "syncToken": sync,
        "prefToken": "PREF-1",
        "contactsOrder": order,
        "contacts": contacts,
        "groups": [{"groupId":"L-REMOTE","name":"Remote list","contactIds":["R-1"],"etag":"g1"}],
        "collections": [{"groupsOrder":["L-REMOTE"]}],
    })
}

fn remote_card(id: &str, first: &str) -> Value {
    json!({
        "contactId": id,
        "firstName": first,
        "lastName": "Remote",
        "etag": format!("etag-{id}"),
        "emailAddresses": [
            {"label":"HOME","field":format!("{id}@example.com")},
            {"label":"WORK","field":format!("{id}@work.example")}
        ],
        "phones": [{"label":"MOBILE","field":"+1 555 0199"}],
        "photo": {"url":"https://p01-contacts.icloud.com/SYNTHETIC-REMOTE-PHOTO","signature":"SIG"},
        "someFieldTheCacheDoesNotKeep": "SYNTHETIC-UNKEPT",
    })
}

async fn apple() -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"ckdatabasews":{"url":remote.uri()},"contacts":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    remote
}

fn saved_client(dir: &std::path::Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":uri},"contacts":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

/// A bound directory with unrelated Notes/Reminders state and queued writes of every service.
fn bound_with_unrelated_state(policy: Option<&str>) -> (tempfile::TempDir, Server, Vec<i64>) {
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
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    let ids = vec![
        library
            .outbox_append("notes", "N-1", "create", "{\"body\":\"queued note\"}")
            .unwrap(),
        library
            .outbox_append("reminders", "R-ITEM", "create", "{\"title\":\"queued\"}")
            .unwrap(),
        library
            .outbox_append("contacts", "C-QUEUED", "delete", "{}")
            .unwrap(),
    ];
    (dir, server, ids)
}

fn unrelated_snapshot(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let mut pending = Vec::new();
    for service in ["notes", "reminders", "contacts"] {
        for p in library.outbox_pending(service).unwrap() {
            pending.push(json!([
                p.id, p.service, p.record, p.kind, p.payload, p.tries
            ]));
        }
    }
    let mut states = Vec::new();
    for service in ["notes", "reminders"] {
        states.push(json!([
            library.sync_token(service).unwrap(),
            library.last_synced(service).unwrap(),
            library
                .state(&format!("mcp.full_reread.{service}"))
                .unwrap(),
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

fn only_contacts_reads(requests: &[wiremock::Request]) {
    for r in requests {
        let p = r.url.path();
        assert!(
            p == "/setup/ws/1/validate"
                || (r.method.as_str() == "GET"
                    && (p == "/co/startup" || p.trim_end_matches('/') == "/co/contacts/card"))
                || (r.method.as_str() == "POST" && p == "/co/changeset"),
            "unexpected {} {p}",
            r.method
        );
    }
}

#[tokio::test]
async fn refresh_reads_only_apple_contacts_and_never_sends_queued_writes() {
    for policy in [None, Some("writes_only"), Some("yolo")] {
        let remote = apple().await;
        Mock::given(method("GET"))
            .and(path("/co/startup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(startup_body(
                json!([remote_card("R-1", "Ines"), remote_card("R-2", "Jun")]),
                "SYNC-1",
            )))
            .mount(&remote)
            .await;
        let (dir, _server, _ids) = bound_with_unrelated_state(policy);
        let before = unrelated_snapshot(dir.path());
        let result = skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri()),
        )
        .await
        .unwrap();
        assert_eq!(result["service"], "contacts", "{policy:?}");
        assert_eq!(result["outcome"], "ok");
        assert_eq!(result["read"], "full");
        assert_eq!(result["cached_contacts"], 2);
        assert_eq!(result["queued_changes_sent"], 0);
        assert_eq!(result["queued_contact_changes"], 1);
        assert_eq!(result["remote_verified"], false);
        assert!(!result.to_string().contains("SYNTHETIC-TOKEN"));
        only_contacts_reads(&remote.received_requests().await.unwrap());
        assert_eq!(unrelated_snapshot(dir.path()), before, "{policy:?}");
        let library = quince_core::Library::open(dir.path()).unwrap();
        let cards = library.contacts(None).unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!(
            cards[0]
                .methods
                .iter()
                .filter(|m| m.kind == "email")
                .count(),
            2
        );
        let cursor = library.sync_token("contacts").unwrap().unwrap();
        assert_eq!(
            quince_icloud::contacts::Tokens::from_stored(&cursor)
                .unwrap()
                .sync,
            "SYNC-1"
        );
        assert_eq!(
            quince_icloud::contacts::stored_lists(&library)[0].name,
            "Remote list"
        );
    }
}

#[tokio::test]
async fn refreshed_contacts_read_back_through_mcp_with_limits_stated() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1")),
        )
        .mount(&remote)
        .await;
    let (dir, mut server, _) = bound_with_unrelated_state(Some("writes_only"));
    skykeep_mcp::contacts::refresh_with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    private_files(dir.path());
    init(&mut server, false).await;
    let got = ok(&mut server, "contacts_get", json!({"guid":"R-1"})).await;
    assert_eq!(got["contact"]["emails"].as_array().unwrap().len(), 2);
    assert_eq!(
        got["contact"]["lists"],
        json!([{"guid":"L-REMOTE","name":"Remote list"}])
    );
    assert_eq!(got["contact"]["photo"]["present"], true);
    let raw = got.to_string();
    assert!(
        !raw.contains("SYNTHETIC-UNKEPT"),
        "unkept fields are not invented"
    );
    assert!(!raw.contains("SYNTHETIC-REMOTE-PHOTO"));
    assert_eq!(got["complete_representation"], false);
}

#[tokio::test]
async fn refresh_uses_the_kept_cursor_and_reads_whole_only_when_apple_moved() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1")),
        )
        .mount(&remote)
        .await;
    let (dir, _server, _) = bound_with_unrelated_state(Some("yolo"));
    let refresh = |dir: std::path::PathBuf, uri: String| async move {
        skykeep_mcp::contacts::refresh_with_client(&dir, saved_client(&dir, &uri)).await
    };
    refresh(dir.path().to_owned(), remote.uri()).await.unwrap();
    remote.reset().await;
    let remote_uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"contacts":{"url":remote_uri}}
        })))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path("/co/changeset"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"syncToken":"SYNC-1","prefToken":"PREF-1"})),
        )
        .mount(&remote)
        .await;
    let unchanged = refresh(dir.path().to_owned(), remote.uri()).await.unwrap();
    assert_eq!(unchanged["read"], "unchanged");
    assert_eq!(unchanged["cached_contacts"], 1);
    let requests = remote.received_requests().await.unwrap();
    only_contacts_reads(&requests);
    assert!(requests.iter().all(|r| r.url.path() != "/co/startup"));
    let changeset = requests
        .iter()
        .find(|r| r.url.path() == "/co/changeset")
        .unwrap();
    assert!(changeset
        .url
        .query_pairs()
        .any(|(k, v)| k == "syncToken" && v == "SYNC-1"));
    // Apple says the book moved (420): the whole book is read again, and a card gone at
    // Apple is gone from the cache.
    remote.reset().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"contacts":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path("/co/changeset"))
        .respond_with(ResponseTemplate::new(420))
        .mount(&remote)
        .await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-2", "Jun")]), "SYNC-2")),
        )
        .mount(&remote)
        .await;
    let moved = refresh(dir.path().to_owned(), remote.uri()).await.unwrap();
    assert_eq!(moved["read"], "full");
    let library = quince_core::Library::open(dir.path()).unwrap();
    let cards = library.contacts(None).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].guid, "R-2");
}

#[tokio::test]
async fn failed_refresh_keeps_cache_and_cursor_and_a_retry_recovers() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1")),
        )
        .up_to_n_times(1)
        .mount(&remote)
        .await;
    let (dir, mut server, _) = bound_with_unrelated_state(Some("writes_only"));
    skykeep_mcp::contacts::refresh_with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    let cursor = |dir: &std::path::Path| {
        quince_core::Library::open(dir)
            .unwrap()
            .sync_token("contacts")
            .unwrap()
    };
    let kept = cursor(dir.path());
    // Force a whole read, which Apple now refuses as busy.
    quince_core::Library::open(dir.path())
        .unwrap()
        .forget_full_sync("contacts")
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(ResponseTemplate::new(503).set_body_string("SYNTHETIC-UPSTREAM-BODY"))
        .up_to_n_times(1)
        .mount(&remote)
        .await;
    let before = unrelated_snapshot(dir.path());
    let failed = skykeep_mcp::contacts::refresh_with_client(
        dir.path(),
        saved_client(dir.path(), &remote.uri()),
    )
    .await
    .unwrap_err();
    assert_eq!(failed, "contacts_refresh_failed_cache_kept");
    assert_eq!(cursor(dir.path()), kept);
    assert_eq!(unrelated_snapshot(dir.path()), before);
    private_files(dir.path());
    init(&mut server, false).await;
    let page = ok(&mut server, "contacts_list", json!({})).await;
    assert_eq!(guids(&page), ["R-1"]);
    assert_eq!(page["cache"]["last_refresh_failed"], true);
    assert!(!page.to_string().contains("SYNTHETIC-UPSTREAM-BODY"));
    // Retried once Apple answers again.
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(startup_body(
            json!([remote_card("R-1", "Ines"), remote_card("R-3", "Kai")]),
            "SYNC-3",
        )))
        .mount(&remote)
        .await;
    let retried = skykeep_mcp::contacts::refresh_with_client(
        dir.path(),
        saved_client(dir.path(), &remote.uri()),
    )
    .await
    .unwrap();
    assert_eq!(retried["cached_contacts"], 2);
    private_files(dir.path());
    let page = ok(&mut server, "contacts_list", json!({})).await;
    assert_eq!(page["cache"]["last_refresh_failed"], false);
}

#[tokio::test]
async fn refresh_fails_closed_on_session_account_or_service_problems() {
    // Apple ends the session for Contacts.
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(ResponseTemplate::new(450))
        .mount(&remote)
        .await;
    let (dir, _server, _) = bound_with_unrelated_state(Some("writes_only"));
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
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
    quince_icloud::contacts::apply(&mut library, synthetic_book()).unwrap();
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "account_mismatch_use_new_state_directory"
    );
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().starts_with("/co/")));
    assert_eq!(
        library.contacts(None).unwrap().len(),
        7,
        "cache left as it was"
    );
    // No Contacts service in the session.
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"ckdatabasews":{"url":remote.uri()}}
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
        skykeep_mcp::contacts::refresh_with_client(dir.path(), client)
            .await
            .unwrap_err(),
        "contacts_service_unavailable"
    );
    // Unbound directory: nothing is resumed or read.
    let remote = apple().await;
    let dir = private_dir();
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "login_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_and_writes_refresh_without_approval_touches_nothing() {
    let remote = apple().await;
    let (dir, _server, _) = bound_with_unrelated_state(Some("reads_and_writes"));
    let before = unrelated_snapshot(dir.path());
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "read_approval_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
    assert_eq!(unrelated_snapshot(dir.path()), before);
}

#[tokio::test]
async fn a_short_address_book_is_refused_not_stored() {
    let remote = apple().await;
    let mut body = startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1");
    body["contactsOrder"] = json!(["R-1", "R-2"]);
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&remote)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/co/contacts/card/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"contacts":[]})))
        .mount(&remote)
        .await;
    let (dir, _server, _) = bound_with_unrelated_state(Some("writes_only"));
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    quince_icloud::contacts::apply(&mut library, synthetic_book()).unwrap();
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "contacts_refresh_failed_cache_kept"
    );
    assert_eq!(library.contacts(None).unwrap().len(), 7);
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

// --- Response size: every emitted page and card fits its cap, measured as sent.

const PAGE_CAP: usize = 262_144;
const CARD_CAP: usize = 524_288;

/// The tool result's text as it is encoded inside the JSON-RPC frame, and the whole frame.
fn emitted(reply: &Value) -> (usize, usize) {
    let text = serde_json::to_string(&reply["result"]["content"][0]["text"]).unwrap();
    (text.len(), serde_json::to_string(reply).unwrap().len())
}

/// Text that grows when encoded: quotes, a control character, markup and multibyte letters.
fn awkward(chars: usize) -> String {
    "\"\u{1}<é東\\".chars().cycle().take(chars).collect()
}

fn huge_book() -> AddressBook {
    let mut contacts = Vec::new();
    let mut long_id = person(&"G".repeat(300_000), Some("Huge"), Some("Longid"));
    long_id.methods = vec![method_of("email", None, "long@example.com")];
    contacts.push(long_id);
    contacts.push(person(&"東".repeat(200), Some("Huge"), Some("Multibyte")));
    contacts.push(person("C-\"\\\u{1}<é>", Some("Huge"), Some("Escaped")));
    for i in 0..40 {
        let mut wide = person(
            &format!("C-WIDE-{i:02}"),
            Some(&format!("Huge{}", awkward(600))),
            Some(&awkward(600)),
        );
        wide.company = Some(awkward(600));
        wide.methods = (0..25)
            .flat_map(|n| {
                [
                    method_of(
                        "email",
                        Some(&awkward(600)),
                        &format!("{n}{}", awkward(600)),
                    ),
                    method_of(
                        "phone",
                        Some(&awkward(600)),
                        &format!("+1 555 {n:04}{}", awkward(600)),
                    ),
                ]
            })
            .collect();
        contacts.push(wide);
    }
    let mut notes = person("C-NOTES", Some("Huge"), Some("Notes"));
    notes.notes = Some(awkward(2_000_000));
    contacts.push(notes);
    let mut many = person("C-MANY", Some("Huge"), Some("Many"));
    many.methods = (0..2000)
        .map(|n| method_of("email", Some("HOME"), &format!("{n}{}", awkward(600))))
        .collect();
    contacts.push(many);
    AddressBook {
        contacts,
        lists: vec![ContactList {
            guid: "L-HUGE".into(),
            name: awkward(300),
            members: vec!["C-NOTES".into()],
            etag: None,
        }],
        me: None,
        addresses: Vec::new(),
        details: Vec::new(),
    }
}

/// Walks every page; each must fit the cap, move forward, and account for every contact.
async fn walk(server: &mut Server, name: &str, mut args: Value) -> Vec<Value> {
    let mut items = Vec::new();
    let mut offset = 0u64;
    let mut total = None;
    loop {
        args["offset"] = json!(offset);
        args["limit"] = json!(100);
        let reply = call(server, 9, name, args.clone()).await;
        assert_eq!(
            reply["result"]["isError"],
            false,
            "{}",
            &reply.to_string()[..300]
        );
        let (text, frame) = emitted(&reply);
        assert!(text <= PAGE_CAP, "{name} page text {text}");
        assert!(frame <= PAGE_CAP + 256, "{name} frame {frame}");
        let page = text_of(&reply);
        total.get_or_insert(page["total"].as_u64().unwrap());
        let got = page["items"].as_array().unwrap();
        assert!(!got.is_empty(), "every page makes progress");
        items.extend(got.iter().cloned());
        match page["next_offset"].as_u64() {
            Some(next) => {
                assert_eq!(next, offset + got.len() as u64);
                assert!(page["page_shortened_to_fit"] == true || got.len() == 100);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(items.len() as u64, total.unwrap(), "no contact skipped");
    items
}

fn text_of(reply: &Value) -> Value {
    text(reply)
}

#[tokio::test]
async fn list_and_search_pages_fit_the_cap_with_huge_identifiers_and_fields() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    quince_icloud::contacts::apply(&mut library, huge_book()).unwrap();
    init(&mut server, false).await;
    for (name, args) in [
        ("contacts_list", json!({})),
        ("contacts_search", json!({"query":"huge"})),
    ] {
        let items = walk(&mut server, name, args).await;
        assert_eq!(items.len(), 45, "{name}");
        // Identifiers too long to address are shown as unsupported, never cut or skipped.
        let unsupported: Vec<&Value> = items.iter().filter(|i| i["guid"].is_null()).collect();
        assert_eq!(unsupported.len(), 2, "{name}");
        for item in &unsupported {
            assert_eq!(item["unsupported"], "guid_too_long");
        }
        let mut lengths: Vec<u64> = unsupported
            .iter()
            .map(|i| i["guid_bytes"].as_u64().unwrap())
            .collect();
        lengths.sort_unstable();
        assert_eq!(lengths, [600, 300_000]);
        // Every other identifier comes back exactly, escaped characters included.
        let ids: Vec<&str> = items.iter().filter_map(|i| i["guid"].as_str()).collect();
        assert!(ids.contains(&"C-\"\\\u{1}<é>"), "{name}");
        assert!(ids.contains(&"C-NOTES") && ids.contains(&"C-MANY"));
        assert_eq!(ids.iter().filter(|g| g.starts_with("C-WIDE-")).count(), 40);
        for item in items
            .iter()
            .filter(|i| i["guid"].as_str().is_some_and(|g| g.starts_with("C-WIDE-")))
        {
            assert_eq!(item["summary_truncated"], true, "{name}");
            assert_eq!(item["emails_total"], 25);
            assert!(item["emails"].as_array().unwrap().len() < 25);
            assert!(serde_json::to_string(&item.to_string()).unwrap().len() <= 32_768);
        }
        let many = items.iter().find(|i| i["guid"] == "C-MANY").unwrap();
        assert_eq!(many["emails_total"], 2000);
        assert_eq!(many["summary_truncated"], true);
    }
}

#[tokio::test]
async fn get_fits_the_cap_by_cutting_fields_it_says_it_cut() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    quince_icloud::contacts::apply(&mut library, huge_book()).unwrap();
    init(&mut server, false).await;
    for guid in ["C-NOTES", "C-MANY", "C-WIDE-07", "C-\"\\\u{1}<é>"] {
        let reply = call(&mut server, 9, "contacts_get", json!({"guid":guid})).await;
        assert_eq!(reply["result"]["isError"], false, "{guid}");
        let (text, frame) = emitted(&reply);
        assert!(text <= CARD_CAP, "{guid} card text {text}");
        assert!(frame <= CARD_CAP + 256, "{guid} frame {frame}");
        let got = text_of(&reply);
        assert_eq!(got["contact"]["guid"], guid, "the identifier is never cut");
    }
    let notes = ok(&mut server, "contacts_get", json!({"guid":"C-NOTES"})).await;
    assert_eq!(notes["card_truncated"], true);
    assert!(notes["contact"]["notes"].as_str().unwrap().chars().count() < 2_000_000);
    assert_eq!(notes["entry_totals"]["lists"], 1);
    let many = ok(&mut server, "contacts_get", json!({"guid":"C-MANY"})).await;
    assert_eq!(many["card_truncated"], true);
    assert_eq!(many["entry_totals"]["emails"], 2000);
    assert!(many["contact"]["emails"].as_array().unwrap().len() < 2000);
    let plain = ok(
        &mut server,
        "contacts_get",
        json!({"guid":"C-\"\\\u{1}<é>"}),
    )
    .await;
    assert_eq!(plain["card_truncated"], false);
    // Identifiers over the request limit cannot be asked for; list shows them as unsupported.
    assert_eq!(
        error(
            &mut server,
            "contacts_get",
            json!({"guid":"東".repeat(200)})
        )
        .await,
        "invalid_arguments"
    );
}

// --- Refresh resource bounds, against a hand-written loopback server.

enum RawBody {
    /// Sent with Content-Length as given (true or not), then the connection closes.
    Plain {
        length: Option<usize>,
        gzip: bool,
        bytes: Vec<u8>,
    },
    /// Chunked transfer: `head`, then `fill` bytes of `a` in 64 KiB chunks, then `tail`.
    Chunked {
        head: Vec<u8>,
        fill: usize,
        tail: Vec<u8>,
        pause_ms: u64,
    },
}

struct Raw {
    uri: String,
    sent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

/// One canned HTTP/1.1 answer per connection, chosen by path. Counts the body bytes the
/// client accepted before it stopped reading.
fn raw_server(answer: impl Fn(&str) -> (u16, RawBody) + Send + Sync + 'static) -> Raw {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let answer = std::sync::Arc::new(answer);
    let (counter, seen) = (sent.clone(), paths.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (answer, counter, seen) = (answer.clone(), counter.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let target = line.split(' ').nth(1).unwrap_or("").to_string();
                let path = target.split('?').next().unwrap_or("").to_string();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = answer(&path);
                let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\nContent-Type: application/json\r\n");
                let write = |stream: &mut std::net::TcpStream, bytes: &[u8]| -> bool {
                    if stream.write_all(bytes).is_err() {
                        return false;
                    }
                    counter.fetch_add(bytes.len(), Ordering::SeqCst);
                    true
                };
                match body {
                    RawBody::Plain {
                        length,
                        gzip,
                        bytes,
                    } => {
                        if let Some(n) = length {
                            head.push_str(&format!("Content-Length: {n}\r\n"));
                        }
                        if gzip {
                            head.push_str("Content-Encoding: gzip\r\n");
                        }
                        head.push_str("\r\n");
                        let _ = stream.write_all(head.as_bytes());
                        let _ = write(&mut stream, &bytes);
                    }
                    RawBody::Chunked {
                        head: start,
                        fill,
                        tail,
                        pause_ms,
                    } => {
                        head.push_str("Transfer-Encoding: chunked\r\n\r\n");
                        let _ = stream.write_all(head.as_bytes());
                        let chunk = |stream: &mut std::net::TcpStream, data: &[u8]| -> bool {
                            stream
                                .write_all(format!("{:x}\r\n", data.len()).as_bytes())
                                .is_ok()
                                && write(stream, data)
                                && stream.write_all(b"\r\n").is_ok()
                        };
                        if !chunk(&mut stream, &start) {
                            return;
                        }
                        let piece = vec![b'a'; 65_536];
                        let mut left = fill;
                        while left > 0 {
                            let n = left.min(piece.len());
                            if pause_ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(pause_ms));
                            }
                            if !chunk(&mut stream, &piece[..n]) {
                                return;
                            }
                            left -= n;
                        }
                        if chunk(&mut stream, &tail) {
                            let _ = stream.write_all(b"0\r\n\r\n");
                        }
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    Raw { uri, sent, paths }
}

/// Apple's sign-in validated by wiremock; Contacts served from `contacts`.
async fn apple_at(contacts: &str) -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"contacts":{"url":contacts}}
        })))
        .mount(&remote)
        .await;
    remote
}

/// A bound directory with the synthetic book cached and unrelated queued writes.
fn cached_with_unrelated_state() -> (tempfile::TempDir, Value) {
    let (dir, _server, _) = bound_with_unrelated_state(Some("writes_only"));
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    quince_icloud::contacts::apply(&mut library, synthetic_book()).unwrap();
    let before = cache_snapshot(dir.path());
    (dir, before)
}

/// Everything a refused refresh must leave alone: cache, cursor and unrelated state.
fn cache_snapshot(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let cards: Vec<String> = library
        .contacts(None)
        .unwrap()
        .iter()
        .map(|c| format!("{}:{:?}", c.guid, c.first_name))
        .collect();
    json!({
        "cards": cards,
        "cursor": library.sync_token("contacts").unwrap(),
        "lists": library.state("contacts.lists").unwrap(),
        "unrelated": unrelated_snapshot(dir),
    })
}

fn small_limits() -> skykeep_mcp::contacts::RefreshLimits {
    skykeep_mcp::contacts::RefreshLimits {
        max_response_bytes: 65_536,
        max_total_bytes: 131_072,
        max_contacts: 2,
        max_card_bytes: 4_096,
        timeout: std::time::Duration::from_secs(10),
    }
}

async fn refresh_limited(
    dir: &std::path::Path,
    sign_in: &MockServer,
    limits: skykeep_mcp::contacts::RefreshLimits,
) -> Result<Value, &'static str> {
    skykeep_mcp::contacts::refresh_with_client_limited(
        dir,
        saved_client(dir, &sign_in.uri()),
        limits,
    )
    .await
}

fn startup_bytes(cards: Value, order: Value) -> Vec<u8> {
    json!({"syncToken":"S-NEW","prefToken":"P-NEW","contactsOrder":order,"contacts":cards})
        .to_string()
        .into_bytes()
}

#[tokio::test]
async fn a_streamed_body_over_the_production_cap_stops_reading_and_keeps_the_cache() {
    let raw = raw_server(|_| {
        (
            200,
            RawBody::Chunked {
                head:
                    br#"{"syncToken":"S","prefToken":"P","contactsOrder":[],"contacts":[],"pad":""#
                        .to_vec(),
                fill: 64 * 1024 * 1024,
                tail: br#""}"#.to_vec(),
                pause_ms: 0,
            },
        )
    });
    let sign_in = apple_at(&raw.uri).await;
    let (dir, before) = cached_with_unrelated_state();
    let error = skykeep_mcp::contacts::refresh_with_client(
        dir.path(),
        saved_client(dir.path(), &sign_in.uri()),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "contacts_response_too_large_cache_kept");
    let sent = raw.sent.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        sent < 48 * 1024 * 1024,
        "reading stopped near the cap, not at the end: {sent}"
    );
    assert_eq!(cache_snapshot(dir.path()), before);
    assert_eq!(*raw.paths.lock().unwrap(), ["/co/startup"]);
}

#[tokio::test]
async fn unannounced_mislabelled_and_compressed_bodies_are_bounded_as_decoded() {
    // No length and no chunking: the body ends when the connection closes.
    let unannounced = raw_server(|_| {
        (
            200,
            RawBody::Plain {
                length: None,
                gzip: false,
                bytes: startup_bytes(json!([]), json!([]))
                    .into_iter()
                    .chain(vec![b' '; 1_000_000])
                    .collect(),
            },
        )
    });
    // A length far below the body: only that much is read, and it is not a book.
    let short_label = raw_server(|_| {
        (
            200,
            RawBody::Plain {
                length: Some(20),
                gzip: false,
                bytes: startup_bytes(json!([remote_card("R-1", "Ines")]), json!(["R-1"])),
            },
        )
    });
    // A length far above the body: the body is cut off, never trusted or preallocated.
    let long_label = raw_server(|_| {
        (
            200,
            RawBody::Plain {
                length: Some(1_000_000_000),
                gzip: false,
                bytes: startup_bytes(json!([remote_card("R-1", "Ines")]), json!(["R-1"])),
            },
        )
    });
    // 1 KiB compressed that decodes to over 1 MiB.
    let compressed = raw_server(|_| {
        (
            200,
            RawBody::Plain {
                length: Some(GZIP_BOMB_LEN),
                gzip: true,
                bytes: gzip_bomb(),
            },
        )
    });
    for (raw, expected) in [
        (&unannounced, "contacts_response_too_large_cache_kept"),
        (&short_label, "contacts_refresh_failed_cache_kept"),
        (&long_label, "contacts_refresh_failed_cache_kept"),
        (&compressed, "contacts_response_too_large_cache_kept"),
    ] {
        let sign_in = apple_at(&raw.uri).await;
        let (dir, before) = cached_with_unrelated_state();
        assert_eq!(
            refresh_limited(dir.path(), &sign_in, small_limits())
                .await
                .unwrap_err(),
            expected,
            "{}",
            raw.uri
        );
        assert_eq!(cache_snapshot(dir.path()), before, "{}", raw.uri);
    }
}

const GZIP_BOMB_LEN: usize = 1091;

/// gzip of `{"syncToken":"S","prefToken":"P","pad":"` + 1 MiB of `a` + `"}`, mtime 0.
fn gzip_bomb() -> Vec<u8> {
    let hex = |s: &str| -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    };
    let mut bytes = hex("1f8b08000000000002ffedc7b10d40401880d15dfeda04a690b0c085d3488e9c4ac4ee284ca07daffabe33f6a38cc3bae4126df4d1c456f3fc7df77e9a9e4a");
    bytes.extend(std::iter::repeat_n(0u8, 1016));
    bytes.extend(hex("3fc5750346bc7f312a001000"));
    assert_eq!(bytes.len(), GZIP_BOMB_LEN);
    bytes
}

#[tokio::test]
async fn a_huge_field_is_refused_whole_with_production_limits() {
    let remote = apple().await;
    let mut card = remote_card("R-1", "Ines");
    card["notes"] = json!("n".repeat(2_000_000));
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(startup_body(json!([card]), "SYNC-1")),
        )
        .mount(&remote)
        .await;
    let (dir, before) = cached_with_unrelated_state();
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "contacts_card_too_large_cache_kept"
    );
    assert_eq!(cache_snapshot(dir.path()), before);
}

#[tokio::test]
async fn card_counts_and_total_bytes_across_pages_are_bounded() {
    // More cards named than the limit: refused before any page is read.
    let raw = raw_server(|_| {
        (
            200,
            RawBody::Plain {
                length: None,
                gzip: false,
                bytes: startup_bytes(
                    json!([remote_card("R-1", "Ines")]),
                    json!(["R-1", "R-2", "R-3"]),
                ),
            },
        )
    });
    let sign_in = apple_at(&raw.uri).await;
    let (dir, before) = cached_with_unrelated_state();
    assert_eq!(
        refresh_limited(dir.path(), &sign_in, small_limits())
            .await
            .unwrap_err(),
        "contacts_book_too_large_cache_kept"
    );
    assert_eq!(*raw.paths.lock().unwrap(), ["/co/startup"]);
    assert_eq!(cache_snapshot(dir.path()), before);
    // Pages that each fit but together pass the total.
    let raw = raw_server(|path| {
        let pad = " ".repeat(60_000);
        let bytes = if path == "/co/startup" {
            format!(
                r#"{{"syncToken":"S","prefToken":"P","contactsOrder":["R-1","R-2"],"contacts":[{}]{pad}}}"#,
                remote_card("R-1", "Ines")
            )
        } else {
            format!(r#"{{"contacts":[{}]{pad}}}"#, remote_card("R-2", "Jun"))
        };
        (
            200,
            RawBody::Plain {
                length: None,
                gzip: false,
                bytes: bytes.into_bytes(),
            },
        )
    });
    let sign_in = apple_at(&raw.uri).await;
    let (dir, before) = cached_with_unrelated_state();
    let mut limits = small_limits();
    limits.max_total_bytes = 100_000;
    assert_eq!(
        refresh_limited(dir.path(), &sign_in, limits)
            .await
            .unwrap_err(),
        "contacts_response_too_large_cache_kept"
    );
    assert_eq!(cache_snapshot(dir.path()), before);
    // Within every limit, the same pages are read and stored.
    let mut limits = small_limits();
    limits.max_total_bytes = 1_000_000;
    let (dir, _) = cached_with_unrelated_state();
    let done = refresh_limited(dir.path(), &sign_in, limits).await.unwrap();
    assert_eq!(done["cached_contacts"], 2);
    assert_eq!(done["read"], "full");
}

#[tokio::test]
async fn an_oversized_changeset_is_refused_without_reading_the_book() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1")),
        )
        .up_to_n_times(1)
        .mount(&remote)
        .await;
    let (dir, _) = cached_with_unrelated_state();
    skykeep_mcp::contacts::refresh_with_client(dir.path(), saved_client(dir.path(), &remote.uri()))
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/co/changeset"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"syncToken":"SYNC-1","prefToken":"PREF-1","pad":"p".repeat(2_000_000)}),
        ))
        .mount(&remote)
        .await;
    let before = cache_snapshot(dir.path());
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "contacts_response_too_large_cache_kept"
    );
    assert_eq!(cache_snapshot(dir.path()), before);
    let startups = remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/co/startup")
        .count();
    assert_eq!(
        startups, 1,
        "the book is not read after an oversized changeset"
    );
}

#[tokio::test]
async fn apple_error_bodies_are_never_read_into_the_library() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(ResponseTemplate::new(422).set_body_string("SYNTHETIC-UPSTREAM-BODY"))
        .mount(&remote)
        .await;
    let (dir, before) = cached_with_unrelated_state();
    assert_eq!(
        skykeep_mcp::contacts::refresh_with_client(
            dir.path(),
            saved_client(dir.path(), &remote.uri())
        )
        .await
        .unwrap_err(),
        "contacts_refresh_failed_cache_kept"
    );
    let library = quince_core::Library::open(dir.path()).unwrap();
    let health = library.sync_health().unwrap();
    let mine = health.iter().find(|h| h.service == "contacts").unwrap();
    let error = mine.last_error.as_deref().unwrap();
    assert!(!error.contains("SYNTHETIC-UPSTREAM-BODY"), "{error}");
    assert_eq!(cache_snapshot(dir.path())["cards"], before["cards"]);
}

#[tokio::test]
async fn a_slow_stream_times_out_and_stores_nothing() {
    let raw = raw_server(|_| {
        (
            200,
            RawBody::Chunked {
                head:
                    br#"{"syncToken":"S","prefToken":"P","contactsOrder":[],"contacts":[],"pad":""#
                        .to_vec(),
                fill: 10 * 65_536,
                tail: br#""}"#.to_vec(),
                pause_ms: 200,
            },
        )
    });
    let sign_in = apple_at(&raw.uri).await;
    let (dir, before) = cached_with_unrelated_state();
    let mut limits = small_limits();
    limits.max_response_bytes = 4 * 1024 * 1024;
    limits.max_total_bytes = 4 * 1024 * 1024;
    limits.timeout = std::time::Duration::from_millis(500);
    let started = std::time::Instant::now();
    assert_eq!(
        refresh_limited(dir.path(), &sign_in, limits)
            .await
            .unwrap_err(),
        "contacts_refresh_timeout_cache_kept"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(cache_snapshot(dir.path()), before);
}

#[tokio::test]
async fn an_account_change_during_refresh_stores_nothing() {
    let remote = apple().await;
    Mock::given(method("GET"))
        .and(path("/co/startup"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(startup_body(json!([remote_card("R-1", "Ines")]), "SYNC-1"))
                .set_delay(std::time::Duration::from_millis(600)),
        )
        .mount(&remote)
        .await;
    let (dir, before) = cached_with_unrelated_state();
    let path = dir.path().to_owned();
    let switch = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        quince_core::Library::open(&path)
            .unwrap()
            .set_state("account", &quince_sync::queue::account_tag("9999999999"))
            .unwrap();
    });
    let error = skykeep_mcp::contacts::refresh_with_client(
        dir.path(),
        saved_client(dir.path(), &remote.uri()),
    )
    .await
    .unwrap_err();
    switch.join().unwrap();
    assert_eq!(error, "account_changed_during_refresh_cache_kept");
    assert_eq!(cache_snapshot(dir.path())["cards"], before["cards"]);
    assert_eq!(cache_snapshot(dir.path())["cursor"], before["cursor"]);
}
