//! Apple iCloud Contacts over the real stdio transport. Kept apart from the in-process
//! tests: a child spawned while another test holds a directory lock briefly inherits it.
use icloud_mcp::Server;
use quince_core::contacts::{Contact, ContactMethod};
use quince_icloud::contacts::{AddressBook, ContactList};
use serde_json::{json, Value};

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn person(
    guid: &str,
    first: Option<&str>,
    last: Option<&str>,
    methods: Vec<ContactMethod>,
) -> Contact {
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
        etag: None,
        methods,
    }
}

fn email(value: &str) -> ContactMethod {
    ContactMethod {
        kind: "email".into(),
        label: Some("HOME".into()),
        value: value.into(),
    }
}

/// A bound directory holding a small synthetic book.
fn seeded(policy: &str) -> tempfile::TempDir {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    let mut acme = person("C-ACME", None, None, Vec::new());
    acme.company = Some("Acme 東京".into());
    acme.is_company = true;
    let book = AddressBook {
        contacts: vec![
            person(
                "C-ADA",
                Some("Ada"),
                Some("Lovelace"),
                vec![
                    email("ada@example.com"),
                    email("ada@engine.example"),
                    email("countess@example.org"),
                ],
            ),
            person("C-ZOE", Some("Zoë"), Some("Ångström"), Vec::new()),
            acme,
            person(
                "C-NUMBER",
                None,
                None,
                vec![ContactMethod {
                    kind: "phone".into(),
                    label: None,
                    value: "+1 555 0100".into(),
                }],
            ),
        ],
        lists: vec![ContactList {
            guid: "L-1".into(),
            name: "Friends".into(),
            members: vec!["C-ADA".into()],
            etag: None,
        }],
        me: None,
        addresses: Vec::new(),
        details: Vec::new(),
    };
    quince_icloud::contacts::apply(&mut library, book).unwrap();
    server.set_policy(policy).unwrap();
    dir
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn guids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["guid"].as_str().unwrap().to_string())
        .collect()
}

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

struct Stdio {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Stdio {
    fn start(dir: &std::path::Path) -> Self {
        use std::io::BufRead;
        use std::process::{Command, Stdio as Pipe};
        private_files(dir);
        let mut child = Command::new(env!("CARGO_BIN_EXE_icloud-mcp"))
            .arg("--state-dir")
            .arg(dir)
            .stdin(Pipe::piped())
            .stdout(Pipe::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut me = Self {
            child,
            input,
            output,
        };
        let init = me.ask(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}}}}}));
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        me
    }

    fn ask(&mut self, message: Value) -> Value {
        use std::io::Write;
        writeln!(self.input, "{message}").unwrap();
        serde_json::from_str(&self.output.next().unwrap().unwrap()).unwrap()
    }

    fn finish(self) {
        let Stdio {
            mut child, input, ..
        } = self;
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn stdio_discovers_and_reads_contacts_with_bound_approval() {
    let dir = seeded("reads_and_writes");
    let mut mcp = Stdio::start(dir.path());
    let list = mcp.ask(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for name in [
        "contacts_list",
        "contacts_search",
        "contacts_get",
        "contacts_refresh",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    let form = mcp.ask(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"contacts_search","arguments":{"query":"zoë"}}}));
    assert_eq!(form["method"], "elicitation/create", "{form}");
    assert_eq!(
        form["params"]["requestedSchema"],
        json!({"type":"object","properties":{"decision":{"type":"string","title":"Decision","oneOf":[{"const":"approve","title":"Approve this time"},{"const":"deny","title":"Deny"},{"const":"always_allow","title":"Always allow this action"}]}},"required":["decision"]})
    );
    let reply =
        mcp.ask(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"decision":"approve"}}}));
    assert_eq!(reply["id"], 3);
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(guids(&text(&reply)), ["C-ZOE"]);
    let form = mcp.ask(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"contacts_get","arguments":{"guid":"C-ADA"}}}));
    let reply = mcp.ask(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}));
    assert_eq!(reply["id"], 4);
    assert_eq!(text(&reply)["error"], "approval_denied");
    assert!(!reply.to_string().contains("Lovelace"));
    mcp.finish();
}

#[test]
fn stdio_reads_without_prompt_in_writes_only_and_refresh_needs_login() {
    let dir = seeded("writes_only");
    let mut mcp = Stdio::start(dir.path());
    let reply = mcp.ask(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"contacts_get","arguments":{"guid":"C-ADA"}}}));
    assert_eq!(reply["id"], 2, "{reply}");
    assert_eq!(
        text(&reply)["contact"]["emails"].as_array().unwrap().len(),
        3
    );
    let reply = mcp.ask(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"contacts_list","arguments":{"limit":2}}}));
    assert_eq!(guids(&text(&reply)), ["C-NUMBER", "C-ACME"]);
    mcp.finish();
    // An unbound directory: refresh stops before any keyring or network use.
    let dir = private_dir();
    Server::open(dir.path())
        .unwrap()
        .set_policy("writes_only")
        .unwrap();
    let mut mcp = Stdio::start(dir.path());
    let reply = mcp.ask(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"contacts_refresh","arguments":{}}}));
    assert_eq!(reply["id"], 2, "{reply}");
    assert_eq!(text(&reply)["error"], "login_required");
    mcp.finish();
}
