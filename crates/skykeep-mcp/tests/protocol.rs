use serde_json::json;
use skykeep_mcp::Server;

#[tokio::test]
async fn note_confirmation_exposes_existing_runs_and_rejects_changed_formatting() {
    let (_dir, mut server, mut library) =
        configured("yolo", json!({"elicitation":{}}), "2025-11-25").await;
    let reply = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"Format body"}),
        ))
        .await
        .unwrap();
    let created: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let guid = created["guid"].as_str().unwrap();
    let bold = json!([{"length":11,"bold":true}]);
    library.set_note_runs(guid, &bold.to_string()).unwrap();
    server.set_policy("writes_only").unwrap();
    let args = json!({"action":"save","guid":guid,"body":"Format body","runs":[{"length":11,"italic":true}]});
    let form = server
        .handle(operation(3, "notes_write", args.clone()))
        .await
        .unwrap();
    let message = form["params"]["message"].as_str().unwrap();
    // The inert display also escapes array punctuation outside string literals.
    let shown = message
        .split_once('\n')
        .unwrap()
        .1
        .replace("\\u005b", "[")
        .replace("\\u005d", "]");
    let preview: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(
        preview["existing_item"]["runs"], bold,
        "the form must include existing formatting separately from Note serialization"
    );
    assert_eq!(preview["operation"]["arguments"]["runs"][0]["italic"], true);
    library
        .set_note_runs(guid, &json!([{"length":11,"underline":true}]).to_string())
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(
        reply["result"]["isError"], true,
        "format-only changes invalidate consent"
    );
    assert!(library.outbox_all_pending().unwrap().is_empty());
    let fresh = server
        .handle(operation(4, "notes_write", args))
        .await
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":fresh["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let pending = library.outbox_all_pending().unwrap();
    let oversized = serde_json::to_string(&vec![json!({"length":11,"bold":true}); 2000]).unwrap();
    library.set_note_runs(guid, &oversized).unwrap();
    let refused = server
        .handle(operation(
            5,
            "notes_write",
            json!({"action":"pin","guid":guid,"pinned":true}),
        ))
        .await
        .unwrap();
    assert_eq!(
        refused["result"]["isError"], true,
        "oversized cached runs must be refused, never truncated"
    );
    assert_eq!(library.outbox_all_pending().unwrap(), pending);
}

#[tokio::test]
async fn confirmation_accepts_empty_content_and_inert_result_metadata_only() {
    for metadata in [
        None,
        Some(json!({"arguments":{"body":"must not replace original"},"action":"decline"})),
    ] {
        let (_dir, mut server, library) =
            configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
        let form = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":"immutable original"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            form["params"]["requestedSchema"],
            json!({"type":"object","properties":{}})
        );
        let mut result = json!({"action":"accept","content":{}});
        if let Some(meta) = metadata {
            result["_meta"] = meta;
        }
        let reply = server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":result}))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        let queued: serde_json::Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            library
                .note_body(queued["guid"].as_str().unwrap())
                .unwrap()
                .as_deref(),
            Some("immutable original")
        );
        assert_eq!(library.outbox_count().unwrap(), 1);
        assert!(server
            .handle(
                json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}})
            )
            .await
            .is_none());
    }
}

#[tokio::test]
async fn confirmation_rejects_missing_nonempty_or_malformed_content_and_ambiguous_envelopes() {
    for result in [
        json!({"action":"accept"}),
        json!({"action":"accept","content":null}),
        json!({"action":"accept","content":[]}),
        json!({"action":"accept","content":false}),
        json!({"action":"accept","content":{"approve":true}}),
        json!({"action":"accept","content":{},"_meta":null}),
        json!({"action":"accept","content":{},"_meta":[]}),
        json!({"action":"accept","content":{},"other":true}),
        json!({"content":{}}),
    ] {
        let (_dir, mut server, library) =
            configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
        let form = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":"must not queue"}),
            ))
            .await
            .unwrap();
        let reply = server
            .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":result}))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
    for extra in [
        json!({"error":{"code":-32603,"message":"ambiguous"}}),
        json!({"params":{}}),
        json!({"method":"tools/call"}),
    ] {
        let (_dir, mut server, library) =
            configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
        let form = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":"must not queue"}),
            ))
            .await
            .unwrap();
        let mut reply =
            json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}});
        reply
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = server.handle(reply).await;
        assert!(response.is_none() || response.unwrap()["result"]["isError"] == true);
        assert_eq!(library.outbox_count().unwrap(), 0);
    }
}

#[tokio::test]
async fn nonempty_cached_pages_remain_incomplete_and_require_parent_for_reminders() {
    let (_dir, mut server, mut library) = configured("yolo", json!({}), "2025-11-25").await;
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    for i in 0..3 {
        library
            .put_reminder_local(&quince_core::reminders::Reminder {
                guid: format!("r{i}"),
                list_guid: "list".into(),
                title: format!("title{i}"),
                ..Default::default()
            })
            .unwrap();
        let reply = server
            .handle(operation(
                2,
                "notes_write",
                json!({"action":"create","body":format!("note{i}")}),
            ))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], false);
    }
    for (name, parent) in [
        ("notes_list", serde_json::Value::Null),
        ("reminders_list", json!("list")),
    ] {
        let mut seen = std::collections::HashSet::new();
        for offset in 0..3 {
            let mut args = json!({"offset":offset,"limit":1});
            if !parent.is_null() {
                args["parent"] = parent.clone();
            }
            let reply = server.handle(operation(3, name, args)).await.unwrap();
            let page: serde_json::Value =
                serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(page["source"], "local_cache");
            assert_eq!(page["complete_representation"], false);
            assert_eq!(page["total"], 3);
            assert!(seen.insert(page["items"][0]["guid"].as_str().unwrap().to_owned()));
            assert_eq!(
                page["next_offset"],
                if offset < 2 {
                    json!(offset + 1)
                } else {
                    serde_json::Value::Null
                }
            );
        }
    }
    assert_eq!(
        server
            .handle(operation(4, "reminders_list", json!({})))
            .await
            .unwrap()["result"]["isError"],
        true
    );
}

#[tokio::test]
async fn configured_note_mutations_require_exact_forms_and_yolo_deletion_does_not() {
    let (_dir, mut server, mut library) =
        configured("yolo", json!({"elicitation":{}}), "2025-11-25").await;
    library
        .put_note_folders(&[quince_core::notes::NoteFolder {
            guid: "folder".into(),
            title: "Target".into(),
            sort_order: 0,
            parent_guid: None,
        }])
        .unwrap();
    let reply = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"full original body"}),
        ))
        .await
        .unwrap();
    let created: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let guid = created["guid"].as_str().unwrap();
    server.set_policy("writes_only").unwrap();
    for args in [
        json!({"action":"save","guid":guid,"body":"updated full body"}),
        json!({"action":"pin","guid":guid,"pinned":true}),
        json!({"action":"move","guid":guid,"folder_guid":"folder"}),
        json!({"action":"delete","guid":guid}),
    ] {
        let action = args["action"].as_str().unwrap().to_owned();
        let count = library.outbox_count().unwrap();
        let form = server
            .handle(operation(3, "notes_write", args))
            .await
            .unwrap();
        assert_eq!(form["method"], "elicitation/create", "{form}");
        assert_eq!(library.outbox_count().unwrap(), count);
        let reply = server
            .handle(
                json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}),
            )
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], false, "{action}: {reply}");
    }
    server.set_policy("yolo").unwrap();
    let reply = server
        .handle(operation(
            4,
            "notes_write",
            json!({"action":"create","body":"yolo delete fixture"}),
        ))
        .await
        .unwrap();
    let created: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let guid = created["guid"].as_str().unwrap();
    let reply = server
        .handle(operation(
            4,
            "notes_write",
            json!({"action":"delete","guid":guid}),
        ))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert!(reply.get("method").is_none());
}

#[tokio::test]
async fn oversized_stdio_frame_is_bounded_before_parsing() {
    let bytes = vec![b'x'; skykeep_mcp::MAX_FRAME_BYTES + 1];
    let mut input = tokio::io::BufReader::new(bytes.as_slice());
    let mut frame = Vec::new();
    assert_eq!(
        skykeep_mcp::read_frame(&mut input, &mut frame)
            .await
            .unwrap_err(),
        "frame_too_large"
    );
    assert!(frame.len() <= skykeep_mcp::MAX_FRAME_BYTES);
}

#[tokio::test]
async fn read_write_policy_requires_read_consent_before_resolved_mutation_preview() {
    let (_dir, mut server, mut library) =
        configured("reads_and_writes", json!({"elicitation":{}}), "2025-11-25").await;
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "private resolved title".into(),
            ..Default::default()
        })
        .unwrap();
    let first = server
        .handle(operation(
            2,
            "reminders_write",
            json!({"action":"delete","guid":"r"}),
        ))
        .await
        .unwrap();
    assert!(!first["params"]["message"]
        .as_str()
        .unwrap()
        .contains("private resolved title"));
    let second = server
        .handle(json!({"jsonrpc":"2.0","id":first["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_ne!(first["id"], second["id"]);
    assert!(second["params"]["message"]
        .as_str()
        .unwrap()
        .contains("private resolved title"));
    assert_eq!(library.outbox_count().unwrap(), 0);
    let reply = server
        .handle(
            json!({"jsonrpc":"2.0","id":second["id"],"result":{"action":"accept","content":{}}}),
        )
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(library.outbox_count().unwrap(), 1);
}

#[tokio::test]
async fn coalesced_payload_is_reauthorized_and_policy_changes_quarantine_it() {
    let (dir, mut server, library) = configured("yolo", json!({}), "2025-11-25").await;
    let reply = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"first"}),
        ))
        .await
        .unwrap();
    let result: serde_json::Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let guid = result["guid"].as_str().unwrap();
    let mut original = None;
    for body in ["second", "final exact body"] {
        let reply = server
            .handle(operation(
                3,
                "notes_write",
                json!({"action":"save","guid":guid,"body":body}),
            ))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        if original.is_none() {
            original = library
                .outbox_pending("notes")
                .unwrap()
                .into_iter()
                .find(|p| p.kind == "save");
        }
    }
    let original = original.unwrap();
    let pending = library.outbox_pending("notes").unwrap();
    assert_eq!(pending.len(), 2); // Create stays ordered before the coalesced save.
    let save = pending.iter().find(|p| p.kind == "save").unwrap();
    assert_ne!(save.payload, original.payload);
    assert!(save.payload.contains("final exact body"));
    assert!(skykeep_mcp::sync::authorized(&library, dir.path(), save).is_ok());
    assert!(skykeep_mcp::sync::authorized(&library, dir.path(), &original).is_err());
    server.set_policy("writes_only").unwrap();
    assert!(library.outbox_pending("notes").unwrap().is_empty());
    assert_eq!(library.outbox_refused().unwrap().len(), 2);
    assert!(skykeep_mcp::sync::authorized(&library, dir.path(), &pending[0]).is_err());
}

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

#[tokio::test]
async fn existing_reminder_preview_is_complete_and_stale_consent_cannot_mutate() {
    let (_dir, mut server, mut library) =
        configured("writes_only", json!({"elicitation":{}}), "2025-11-25").await;
    library
        .put_reminder_lists(&[quince_core::reminders::ReminderList {
            guid: "list".into(),
            ..Default::default()
        }])
        .unwrap();
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "full existing title".into(),
            ..Default::default()
        })
        .unwrap();
    let args = json!({"action":"delete","guid":"r"});
    let form = server
        .handle(operation(2, "reminders_write", args.clone()))
        .await
        .unwrap();
    assert_eq!(form["method"], "elicitation/create");
    assert!(form["params"]["message"]
        .as_str()
        .unwrap()
        .contains("full existing title"));
    library
        .put_reminder_local(&quince_core::reminders::Reminder {
            guid: "r".into(),
            list_guid: "list".into(),
            title: "changed while awaiting consent".into(),
            ..Default::default()
        })
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(library.outbox_count().unwrap(), 0);
    let form = server
        .handle(operation(3, "reminders_write", args))
        .await
        .unwrap();
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(library.outbox_count().unwrap(), 1);
}

#[tokio::test]
async fn configured_mutation_persists_exact_payload_authorization() {
    let (_dir, mut server, library) = configured("yolo", json!({}), "2025-11-25").await;
    let reply = server
        .handle(operation(
            2,
            "notes_write",
            json!({"action":"create","body":"exact durable body"}),
        ))
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false);
    let pending = library.outbox_pending("notes").unwrap().remove(0);
    let raw = library
        .state(&format!("mcp.authorization.{}", pending.id))
        .unwrap();
    assert!(
        raw.is_some(),
        "enqueue and durable authorization must commit together"
    );
    let record: serde_json::Value = serde_json::from_str(&raw.unwrap()).unwrap();
    assert_eq!(record["pending"]["payload"], pending.payload);
    assert_eq!(
        record["policy"],
        library.state("mcp.policy").unwrap().unwrap()
    );
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
    let accept = json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}});
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
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
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
            .handle(json!({"id":2,"result":{"action":"accept","content":{}}}))
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
        assert!(server
            .handle(
                json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}})
            )
            .await
            .is_none());
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
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
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
    assert!(server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .is_none());
    assert_eq!(library.outbox_count().unwrap(), 0);
}

#[tokio::test]
async fn unsolicited_consent_responses_are_never_requests() {
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::open(dir.path()).unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"result":{"action":"accept","content":{}}}),
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
    assert!(!names
        .iter()
        .any(|n| ["secret", "password", "code", "download", "upload"]
            .iter()
            .any(|s| n.contains(s))));
    // Photos are read-only metadata in this phase.
    let photos: Vec<_> = names.iter().filter(|n| n.contains("photos")).collect();
    assert_eq!(
        photos,
        [
            &"photos_albums",
            &"photos_list",
            &"photos_search",
            &"photos_get",
            &"photos_refresh"
        ]
    );
    // Contacts are read-only in this phase.
    let contacts: Vec<_> = names.iter().filter(|n| n.contains("contacts")).collect();
    assert_eq!(
        contacts,
        [
            &"contacts_list",
            &"contacts_search",
            &"contacts_get",
            &"contacts_refresh"
        ]
    );
    // Calendar is read-only in this phase.
    let calendar: Vec<_> = names.iter().filter(|n| n.contains("calendar")).collect();
    assert_eq!(
        calendar,
        [
            &"calendar_calendars",
            &"calendar_events",
            &"calendar_search",
            &"calendar_get",
            &"calendar_refresh"
        ]
    );
    // Drive: four reads and one tool for every change.
    let drive: Vec<_> = names.iter().filter(|n| n.contains("drive")).collect();
    assert_eq!(
        drive,
        [
            &"drive_list",
            &"drive_get",
            &"drive_search",
            &"drive_refresh",
            &"drive_write"
        ]
    );
    assert!(server
        .handle(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}})
        )
        .await
        .is_none());
}

#[tokio::test]
async fn initialize_and_tools_name_apple_icloud_notes_and_reminders() {
    for version in ["2025-03-26", "2025-11-25"] {
        let dir = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut server = Server::open(dir.path()).unwrap();
        let init = server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], version);
        let instructions = init["result"]["instructions"].as_str().unwrap();
        for phrase in [
            "Apple iCloud Notes",
            "Apple Reminders",
            "local",
            "approval",
            "terminal",
        ] {
            assert!(instructions.contains(phrase), "{version}: {phrase}");
        }
        // `title` on Implementation exists only from protocol 2025-06-18 on.
        let title = &init["result"]["serverInfo"]["title"];
        if version == "2025-11-25" {
            let title = title.as_str().unwrap();
            assert!(title.contains("Apple iCloud Notes") && title.contains("Reminders"));
        } else {
            assert!(title.is_null(), "{version}: unexpected title");
        }
        let list = server
            .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await
            .unwrap();
        for tool in list["result"]["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let description = tool["description"].as_str().unwrap();
            if name.starts_with("notes_") {
                assert!(description.contains("Apple iCloud Notes"), "{name}");
            } else if name.starts_with("reminders_") {
                assert!(description.contains("Apple Reminders"), "{name}");
            } else {
                assert!(description.contains("Apple"), "{name}");
            }
        }
    }
}
