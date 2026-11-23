//! Error replies say what was wrong and what to do next, so an AI app can fix its own call.
//! Synthetic data only, no network.
use serde_json::{json, Value};
use skykeep_mcp::Server;
use std::os::unix::fs::PermissionsExt;

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A folder with no approval mode, so reads run without a card. `account` binds one.
async fn server(dir: &std::path::Path, account: bool) -> Server {
    let mut server = Server::open(dir).unwrap();
    if account {
        quince_core::Library::open(dir)
            .unwrap()
            .set_state("account", "synthetic-account")
            .unwrap();
    }
    server
        .handle(json!({"id":0,"method":"initialize"}))
        .await
        .unwrap();
    server
}

/// The tool's reply as JSON, and whether it is an error.
async fn call(server: &mut Server, name: &str, args: Value) -> (Value, bool) {
    let reply = server
        .handle(json!({"id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap();
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    (
        serde_json::from_str(text).unwrap(),
        reply["result"]["isError"] == true,
    )
}

fn instructions(reply: &Value) -> &str {
    reply["instructions"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"))
}

#[tokio::test]
async fn invalid_arguments_names_the_missing_field() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let (reply, failed) = call(&mut s, "notes_get", json!({})).await;
    assert!(failed);
    assert_eq!(reply["error"], "invalid_arguments");
    let text = instructions(&reply);
    assert!(text.contains("notes_get"), "{text}");
    assert!(text.contains("guid"), "{text}");
    assert!(text.contains("missing"), "{text}");
}

#[tokio::test]
async fn invalid_arguments_names_an_unknown_field_and_a_wrong_type() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let (reply, _) = call(&mut s, "contacts_list", json!({"color":"red"})).await;
    assert_eq!(reply["error"], "invalid_arguments");
    assert!(instructions(&reply).contains("color"), "{reply}");
    let (reply, _) = call(&mut s, "notes_list", json!({"limit":"ten"})).await;
    assert_eq!(reply["error"], "invalid_arguments");
    assert!(instructions(&reply).contains("limit"), "{reply}");
}

#[tokio::test]
async fn reminders_delete_and_tick_say_they_take_no_base_etag() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    for action in ["delete", "tick"] {
        let args = json!({"action":action,"guid":"r1","base_etag":"e1"});
        let (reply, failed) = call(&mut s, "reminders_write", args).await;
        assert!(failed);
        assert_eq!(reply["error"], "invalid_arguments");
        let text = instructions(&reply);
        assert!(text.contains("base_etag"), "{text}");
        assert!(
            text.contains(&format!("{action} takes no base_etag")),
            "{text}"
        );
    }
}

#[tokio::test]
async fn parent_required_unknown_tool_and_not_found_say_what_to_do() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let (reply, _) = call(&mut s, "reminders_list", json!({})).await;
    assert_eq!(reply["error"], "parent_required");
    let text = instructions(&reply);
    assert!(
        text.contains("parent") && text.contains("reminders_lists"),
        "{text}"
    );

    let (reply, _) = call(&mut s, "notes_explode", json!({})).await;
    assert_eq!(reply["error"], "unknown_tool");
    let text = instructions(&reply);
    assert!(
        text.contains("notes_explode") && text.contains("tools/list"),
        "{text}"
    );

    let (reply, _) = call(&mut s, "notes_get", json!({"guid":"missing"})).await;
    assert_eq!(reply["error"], "not_found");
    let text = instructions(&reply);
    // Right after a create the item is only in iCloud until the next sync or refresh.
    assert!(text.contains("sync"), "{text}");
    assert!(text.contains("after a create"), "{text}");
}

#[test]
fn unlocking_secure_storage_has_a_step() {
    let dir = private_dir();
    let text = skykeep_mcp::next_step(
        "login_required_unlock_secure_storage",
        dir.path(),
        &Value::Null,
    )
    .unwrap();
    assert!(text.contains("unlock"), "{text}");
    assert!(text.contains("skykeep-mcp"), "{text}");
}

#[tokio::test]
async fn reads_without_a_sign_in_say_not_signed_in() {
    let dir = private_dir();
    let mut s = server(dir.path(), false).await;
    for (name, args) in [
        ("notes_list", json!({})),
        ("notes_folders", json!({})),
        ("reminders_lists", json!({})),
        ("notes_search", json!({"query":"x"})),
        ("notes_get", json!({"guid":"x"})),
        ("contacts_list", json!({})),
        ("drive_list", json!({})),
        ("photos_list", json!({})),
        ("calendar_calendars", json!({})),
    ] {
        let (reply, failed) = call(&mut s, name, args).await;
        assert!(failed, "{name}: {reply}");
        assert_eq!(reply["error"], "not_signed_in", "{name}: {reply}");
        assert!(
            reply["message"].as_str().unwrap().contains("Not signed in"),
            "{name}: {reply}"
        );
        assert!(
            instructions(&reply).contains("skykeep-mcp setup"),
            "{reply}"
        );
    }
    // Signed in, an empty cache is an empty list.
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let (reply, failed) = call(&mut s, "notes_list", json!({})).await;
    assert!(!failed, "{reply}");
    assert_eq!(reply["total"], 0);
}

#[tokio::test]
async fn auth_status_says_signed_in_once_a_sync_worked() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "session_unverified");
    // The first check it suggests reads only and sends nothing.
    let text = instructions(&reply);
    assert!(
        text.starts_with("To check it, run contacts_refresh"),
        "{text}"
    );
    // A sync or refresh reached Apple with this sign-in.
    skykeep_mcp::note_session_check(dir.path(), Ok(()));
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "signed_in", "{reply}");
    assert_eq!(reply["session_checked"], true);
    assert!(reply["checked_at"].as_u64().unwrap() > 0, "{reply}");
    let text = reply["message"].as_str().unwrap();
    assert!(text.starts_with("Signed in as of the last check"), "{text}");
    assert!(text.contains("may have changed since"), "{text}");
    assert!(!reply.to_string().contains("setup"), "{reply}");
    // A later sign-in failure counts over it.
    skykeep_mcp::note_session_check(dir.path(), Err("login_required"));
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "login_required", "{reply}");
    assert_eq!(reply["reason"], "login_required", "{reply}");
    assert!(
        instructions(&reply).contains("skykeep-mcp setup"),
        "{reply}"
    );
}

/// A failed check keeps its reason: a locked keyring or an unfinished account says its own
/// step, not "expired, run setup".
#[tokio::test]
async fn auth_status_keeps_the_reason_a_check_failed() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    skykeep_mcp::note_session_check(dir.path(), Err("login_required_unlock_secure_storage"));
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "locked", "{reply}");
    let text = instructions(&reply);
    assert!(text.contains("unlock the keyring"), "{text}");
    assert!(text.contains("skykeep-mcp unlock"), "{text}");
    assert!(
        !reply["message"].as_str().unwrap().contains("expired"),
        "{reply}"
    );
    skykeep_mcp::note_session_check(dir.path(), Err("login_required_complete_account_setup"));
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "login_required", "{reply}");
    let text = instructions(&reply);
    assert!(text.contains("icloud.com"), "{text}");
    assert!(
        !reply["message"].as_str().unwrap().contains("expired"),
        "{reply}"
    );
}

/// After logout the account link stays, but auth_status says signed out, until the next
/// sign-in.
#[tokio::test]
async fn auth_status_says_signed_out_after_logout() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    skykeep_mcp::note_session_check(dir.path(), Ok(()));
    skykeep_mcp::note_signed_out(dir.path());
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "signed_out", "{reply}");
    assert!(
        instructions(&reply).contains("skykeep-mcp setup"),
        "{reply}"
    );
}

/// A check recorded for one account says nothing once the folder is bound to another.
#[tokio::test]
async fn a_check_counts_only_for_the_account_it_was_made_for() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    skykeep_mcp::note_session_check(dir.path(), Ok(()));
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", "another-synthetic-account")
        .unwrap();
    let (reply, _) = call(&mut s, "auth_status", json!({})).await;
    assert_eq!(reply["status"], "session_unverified", "{reply}");
}

/// Sign-in steps say `setup` everywhere: the tools and auth_status agree.
#[test]
fn login_required_and_auth_status_name_the_same_command() {
    let dir = private_dir();
    let text = skykeep_mcp::next_step("login_required", dir.path(), &Value::Null).unwrap();
    assert!(text.contains("skykeep-mcp setup"), "{text}");
    assert!(!text.contains("skykeep-mcp login"), "{text}");
}

#[tokio::test]
async fn tool_descriptions_say_when_to_read_again_and_what_needs_a_refresh() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let list = s
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name}"))
            .to_string()
    };
    for name in [
        "notes_write",
        "reminders_write",
        "notes_get",
        "reminders_get",
    ] {
        assert!(
            tool(name).contains("can change once more after a create or a sync"),
            "{name}"
        );
    }
    assert!(tool("reminders_write").contains("delete and tick take no base_etag"));
    for name in ["photos_list", "photos_search", "photos_get"] {
        assert!(tool(name).contains("only after photos_refresh"), "{name}");
    }
    for name in ["drive_list", "drive_get", "drive_search"] {
        assert!(tool(name).contains("only after drive_refresh"), "{name}");
    }
}

#[tokio::test]
async fn the_server_introduces_itself_as_icloud_mcp() {
    let dir = private_dir();
    let mut s = Server::open(dir.path()).unwrap();
    let reply = s
        .handle(json!({"id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let result = &reply["result"];
    assert!(result["serverInfo"]["title"]
        .as_str()
        .unwrap()
        .starts_with("iCloud-MCP"));
    assert!(result["instructions"]
        .as_str()
        .unwrap()
        .starts_with("iCloud-MCP"));
    assert!(!result.to_string().contains("Skykeep"), "{result}");
}

/// invalid_arguments names the field for the Drive, Contacts, Calendar and Photos tools too,
/// and a call with two mistakes hears about both.
#[tokio::test]
async fn invalid_arguments_names_the_field_for_every_service() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let cases = [
        (
            "drive_write",
            json!({"action":"rename"}),
            vec!["path or id", "name"],
        ),
        (
            "drive_list",
            json!({"path":"/","id":"FOLDER::x"}),
            vec!["not both"],
        ),
        ("drive_get", json!({}), vec!["exactly one of path or id"]),
        ("contacts_get", json!({"guid":""}), vec!["guid"]),
        ("calendar_events", json!({}), vec!["from", "to", "timezone"]),
        (
            "calendar_refresh",
            json!({"from":"2026-01-01"}),
            vec!["both from and to"],
        ),
        ("photos_get", json!({}), vec!["id"]),
        (
            "auth_status",
            json!({"extra":1}),
            vec!["takes no arguments", "extra"],
        ),
        (
            "notes_write",
            json!({"colour":"red"}),
            vec!["action", "colour"],
        ),
    ];
    for (name, args, words) in cases {
        let (reply, failed) = call(&mut s, name, args.clone()).await;
        assert!(failed, "{name}: {reply}");
        assert_eq!(
            reply["error"], "invalid_arguments",
            "{name} {args}: {reply}"
        );
        let text = instructions(&reply);
        for word in words {
            assert!(text.contains(word), "{name} {args} lacks {word}: {text}");
        }
        assert!(!text.contains("its fields are ."), "{text}");
    }
}

/// Every tool that answers invalid_arguments to a field it doesn't have names that field.
#[tokio::test]
async fn every_tool_names_a_field_it_does_not_have() {
    let dir = private_dir();
    let mut s = server(dir.path(), true).await;
    let list = s
        .handle(json!({"id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let mut checked = 0;
    for tool in list["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let (reply, failed) = call(&mut s, name, json!({"zz_not_a_field":1})).await;
        assert!(failed, "{name}: {reply}");
        if reply["error"] != "invalid_arguments" {
            continue;
        }
        checked += 1;
        assert!(
            instructions(&reply).contains("zz_not_a_field"),
            "{name}: {reply}"
        );
    }
    assert!(
        checked >= 25,
        "only {checked} tools answered invalid_arguments"
    );
}
