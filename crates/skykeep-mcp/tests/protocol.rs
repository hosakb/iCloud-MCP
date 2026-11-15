use serde_json::json;
use skykeep_mcp::Server;

#[tokio::test]
async fn oversized_operations_and_expanding_previews_are_rejected_without_queueing() {
    for (mode, body) in [
        ("yolo", "x".repeat(40000)),
        ("writes_only", "<".repeat(30000)),
    ] {
        let (_dir, mut server, library) =
            configured(mode, json!({"elicitation":{}}), "2025-11-25").await;
        let reply = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":body}),
            ))
            .await
            .unwrap();
        assert_eq!(
            reply["result"]["isError"], true,
            "oversized exact operation must be refused, never truncated"
        );
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
}

async fn configured(
    mode: &str,
    capabilities: serde_json::Value,
    version: &str,
) -> (tempfile::TempDir, Server, quince_core::Library) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.set_state("account", "synthetic").unwrap();
    server.set_policy(mode).unwrap();
    server.handle(json!({"id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":capabilities}})).await;
    (dir, server, library)
}

fn operation(id: i32, name: &str, args: serde_json::Value) -> serde_json::Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

#[tokio::test]
async fn invalid_rpc_id_never_enqueues_in_yolo() {
    let (_dir, mut server, library) = configured("yolo", json!({}), "2025-11-25").await;
    let mut request = operation(
        2,
        "notes_write",
        json!({"action":"create","body":"must not queue"}),
    );
    request["id"] = json!({"invalid":true});
    let reply = server.handle(request).await.unwrap();
    assert_eq!(reply["error"]["code"], -32600);
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn persisted_policy_cannot_follow_an_account_change() {
    let (_dir, mut server, mut library) = configured("yolo", json!({}), "2025-11-25").await;
    library.set_state("account", "different-account").unwrap();
    let reply = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"must not queue"}),
        ))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn copied_policy_cannot_enable_another_state_directory() {
    let (_first_dir, _first, first) = configured("yolo", json!({}), "2025-11-25").await;
    let (_second_dir, mut second, mut library) =
        configured("writes_only", json!({}), "2025-11-25").await;
    library
        .set_state("mcp.policy", &first.state("mcp.policy").unwrap().unwrap())
        .unwrap();
    let reply = second
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"must not queue"}),
        ))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn complete_preview_escapes_markup_and_invisible_unicode() {
    let (_dir, mut server, library) =
        configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
    let body = format!(
        "<script>[click](https://invalid/) `code` \u{202e}{}TAIL",
        "x".repeat(20000)
    );
    let form = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":body}),
        ))
        .await
        .unwrap();
    let preview = form["params"]["message"].as_str().unwrap();
    assert!(preview.contains("TAIL"));
    assert!(!preview.contains('`'));
    assert!(!preview.contains('\u{202e}'));
    assert!(!preview.contains("<script>"));
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn accepted_operation_is_single_use_and_legacy_queue_stays_quarantined() {
    let (dir, mut server, library) =
        configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
    let form = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"exact accepted body"}),
        ))
        .await
        .unwrap();
    let accept = json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"approve":true}}});
    let reply = server.handle(accept.clone()).await.unwrap();
    let result: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "queued");
    assert_eq!(
        library
            .note_body(result["guid"].as_str().unwrap())
            .unwrap()
            .as_deref(),
        Some("exact accepted body")
    );
    assert!(server.handle(accept).await.is_none());
    assert_eq!(library.outbox_count().unwrap(), 1);
    server.set_policy("yolo").unwrap();
    drop(server);
    let mut reopened = Server::open(dir.path()).unwrap();
    assert!(reopened.refresh().await.is_err());
    assert_eq!(library.outbox_count().unwrap(), 1);
}

#[tokio::test]
async fn read_consent_is_required_before_returning_cached_content() {
    let (_dir, mut server, library) =
        configured("reads_and_writes", json!({"elicitation":{}}), "2025-11-25").await;
    let form = server
        .handle(operation(2, "notes_list", json!({})))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create");
    let reply = server.handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"approve":true}}})).await.unwrap();
    assert_eq!(reply["id"], 2);
    assert_eq!(reply["result"]["isError"], false);
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn configured_mode_table_and_background_gates() {
    for mode in ["reads_and_writes", "writes_only", "yolo"] {
        let (_dir, mut server, library) =
            configured(mode, json!({"elicitation":{}}), "2025-11-25").await;
        assert!(server.refresh().await.is_err());
        let read = server
            .handle(operation(2, "notes_list", json!({})))
            .await
            .unwrap();
        assert_eq!(
            read["method"] == "elicitation/create",
            mode == "reads_and_writes"
        );
        if mode == "reads_and_writes" {
            server
                .handle(json!({"id":read["id"],"result":{"action":"decline"}}))
                .await;
        }
        let write = server
            .handle(operation(
                3,
                "notes_write",
                json!({"action":"create","body":"exact"}),
            ))
            .await
            .unwrap();
        assert_eq!(write["method"] == "elicitation/create", mode != "yolo");
        assert_eq!(library.outbox_count().unwrap(), usize::from(mode == "yolo"));
        if mode != "yolo" {
            server
                .handle(json!({"id":write["id"],"result":{"action":"cancel"}}))
                .await;
        }
        assert!(server.refresh().await.is_err());
    }
}

#[tokio::test]
async fn consent_decline_cancel_malformed_mismatch_replay_and_policy_change() {
    for result in [
        json!({"action":"decline"}),
        json!({"action":"cancel"}),
        json!({"action":"accept"}),
        json!({"action":"accept","content":{"approve":false}}),
        json!({"action":"accept","content":{"approve":"true"}}),
    ] {
        let (_dir, mut server, library) = configured(
            "writes_only",
            json!({"elicitation":{"form":{}}}),
            "2025-11-25",
        )
        .await;
        let form = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":"exact"}),
            ))
            .await
            .unwrap();
        assert!(server
            .handle(json!({"id":2,"result":{"action":"accept","content":{"approve":true}}}))
            .await
            .is_none());
        assert_eq!(
            server
                .handle(json!({"id":9,"method":"ping"}))
                .await
                .unwrap()["id"],
            9
        );
        let denied = server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":result}))
            .await
            .unwrap();
        assert_eq!(denied["id"], 2);
        assert_eq!(denied["result"]["isError"], true);
        assert!(server.handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"approve":true}}})).await.is_none());
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
    let (_dir, mut server, library) =
        configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
    let form = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"exact"}),
        ))
        .await
        .unwrap();
    server.set_policy("yolo").unwrap();
    let reply = server.handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"approve":true}}})).await.unwrap();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn incapable_clients_fail_closed_and_pending_calls_cancel() {
    for (version, caps) in [
        ("2025-03-26", json!({"elicitation":{}})),
        ("2025-11-25", json!({})),
        ("2025-11-25", json!({"elicitation":{"url":{}}})),
        ("2025-11-25", json!({"elicitation":true})),
        ("2025-11-25", json!({"elicitation":{"future":{}}})),
    ] {
        let (_dir, mut server, library) = configured("writes_only", caps, version).await;
        let reply = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":"exact"}),
            ))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
    let (dir, mut server, library) =
        configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
    let form = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"exact"}),
        ))
        .await
        .unwrap();
    // No lock is retained while waiting for the human.
    let held = skykeep_mcp::state::PrivateState::open(dir.path()).unwrap();
    drop(held);
    let cancelled = server
        .handle(json!({"method":"notifications/cancelled","params":{"requestId":2}}))
        .await
        .unwrap();
    assert_eq!(cancelled["id"], 2);
    assert!(server.handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{"approve":true}}})).await.is_none());
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn unsolicited_consent_responses_are_never_requests() {
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"result":{"action":"accept","content":{"approve":true}}}),
        json!({"jsonrpc":"2.0","id":"approval-1","error":{"code":-32603,"message":"cancelled"}}),
    ] {
        assert!(
            server.handle(message).await.is_none(),
            "responses must not generate response loops"
        );
    }
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    assert_eq!(
        list["error"]["code"], -32601,
        "response cannot initialize the session"
    );
}

#[tokio::test]
async fn initialize_and_discover_only_notes_and_reminders_tools() {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut server = Server::open(dir.path()).unwrap();
    let init = server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "skykeep-mcp");
    assert!(server
        .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await
        .is_none());
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"notes_list"));
    assert!(names.contains(&"reminders_list"));
    assert!(!names.iter().any(|n| [
        "photos", "drive", "calendar", "contacts", "secret", "password", "code"
    ]
    .iter()
    .any(|s| n.contains(s))));
    assert!(server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}})
        )
        .await
        .is_none());
}
