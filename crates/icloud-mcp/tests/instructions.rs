//! Every error code in the server's source either has an `instructions` line or is listed
//! below as needing none, with the reason. A new code fails this test until it is given one
//! or listed.
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;

/// Codes that need no `instructions` line, grouped by the reason. Keep each group sorted.
const NEEDS_NONE: &[(&str, &[&str])] = &[
    (
        "The code itself names the next step.",
        &[
            "account_mismatch_use_new_state_directory",
            "ambiguous_path_use_id",
            "calendar_paused_approve_on_apple_device",
            "calendar_write_not_confirmed_run_calendar_refresh",
            "contacts_paused_approve_on_apple_device",
            "drive_folder_changed_not_trashed_run_drive_refresh",
            "drive_item_changed_run_drive_refresh",
            "drive_name_not_usable_locally_give_local",
            "drive_paused_approve_on_apple_device",
            "drive_write_not_confirmed_run_drive_refresh",
            "early_alert_account_unavailable_set_one_on_an_apple_device_then_sync",
            "folder_not_in_cache_use_path",
            "invalid_arguments_use_move_for_another_calendar",
            "login_required_or_network_unavailable",
            "logout_pending_unlock_keyring_and_retry",
            "not_found_at_apple_run_calendar_refresh",
            "not_found_at_apple_run_drive_refresh",
            "note_body_unavailable_sync_first",
            "quarantine_full_retire_in_terminal",
            "repeating_reminder_complete_with_tick",
            "state_busy_retry",
        ],
    ),
    (
        "contacts_write's argument codes: the line names the field, so it is made from the call's arguments (tests/contacts_write.rs checks it there).",
        &[
            "contact_create_needs_only_fields",
            "contact_delete_needs_only_guid_and_etag",
            "contact_entry_must_be_label_and_value",
            "contact_field_must_be_text_or_null",
            "contact_invalid_action",
            "contact_invalid_guid_or_etag",
            "contact_is_company_must_be_true_or_false",
            "contact_unknown_field",
            "contact_update_needs_guid_etag_and_fields",
        ],
    ),
    (
        "Apple failed, sent too much or took too long, and the code says nothing changed. The only step is to ask again later.",
        &[
            "account_changed_during_refresh_cache_kept",
            "calendar_read_failed_nothing_sent",
            "calendar_refresh_failed_cache_kept",
            "calendar_refresh_timeout_cache_kept",
            "calendar_response_too_large_cache_kept",
            "calendar_timed_out_nothing_sent",
            "calendar_too_many_events_cache_kept",
            "contacts_book_too_large_cache_kept",
            "contacts_card_too_large_cache_kept",
            "contacts_refresh_failed_cache_kept",
            "contacts_refresh_timeout_cache_kept",
            "contacts_response_too_large_cache_kept",
            "drive_download_failed_nothing_kept",
            "drive_download_incomplete_nothing_kept",
            "drive_download_timed_out_nothing_kept",
            "drive_folder_too_large_cache_kept",
            "drive_item_too_large_cache_kept",
            "drive_refresh_failed_cache_kept",
            "drive_refresh_timeout_cache_kept",
            "drive_response_too_large_cache_kept",
            "photos_bad_continuation_cache_kept",
            "photos_library_preparing_cache_kept",
            "photos_library_unavailable_cache_kept",
            "photos_refresh_timeout_cache_kept",
            "photos_response_too_large_cache_kept",
            "service_unavailable",
            "sync_timeout_delivery_uncertain",
            "transport_failed",
        ],
    ),
    (
        "An approval or permission outcome. The reply's status or message already says what was decided or what waits.",
        &[
            "approval_cancelled",
            "approval_denied",
            "approval_not_found",
            "approval_pending",
            "approval_policy_changed",
            "approval_refused",
            "approval_required",
            "approval_required_or_stale",
            "approval_requires_form_client",
            "approval_requires_newer_protocol",
            "approval_stale_calendar_changed_ask_again",
            "approval_stale_drive_changed_ask_again",
            "approval_timeout",
            "authorization_too_large",
            "configured_policy_required",
            "denied_by_permissions",
            "invalid_authorization",
            "invalid_request_id",
            "read_approval_required",
            "write_authorization_stale",
            "write_mode_stricter",
            "write_refused",
        ],
    ),
    (
        "A fault on this computer or in the private folder. icloud-mcp status reports it with the step to take. unsafe_state_contents has a line only when the folder holds such an entry.",
        &[
            "account_mismatch",
            "clock_unavailable",
            "invalid_cached_runs",
            "invalid_cached_save",
            "invalid_exchange_setting",
            "invalid_policy",
            "legacy_approval_quarantined",
            "legacy_write_quarantined",
            "library_unavailable",
            "permissions_file_unavailable",
            "permissions_log_full",
            "permissions_log_unavailable",
            "permissions_setup_required",
            "policy_unavailable",
            "queue_too_large",
            "search_unavailable",
            "secure_storage_unavailable",
            "staging_unavailable",
            "state_must_be_owned_and_private",
            "state_unavailable",
            "timezone_database_unavailable",
            "unsafe_state_contents",
            "unsafe_state_path",
        ],
    ),
    (
        "Only terminal commands return these, and the terminal prints its own sentence for each (checked below).",
        &[
            "account_locked_visit_account_website",
            "apple_answer_unreadable",
            "auth_cooldown_try_later",
            "authentication_unavailable",
            "code_incorrect_or_expired",
            "credentials_rejected",
            "exchange_limit_out_of_range",
            "exchange_parent_missing",
            "exchange_path_required",
            "foreground_terminal_required",
            "home_folder_unknown_give_path",
            "input_cancelled",
            "input_timeout",
            "interactive_terminal_required",
            "invalid_input",
            "invalid_policy_mode",
            "login_incomplete",
            "pending_changed",
            "permissions_file_unusable",
            "policy_change_cancelled",
            "policy_mode_required",
            "quarantine_retire_cancelled",
            "regional_login_retry_required",
            "security_key_sign_in_unsupported",
            "sign_in_wait_protects_account",
            "state_directory_required",
            "storage_already_chosen",
            "terminal_unavailable",
            "too_many_code_requests_try_later",
            "unexpected_argument",
            "use_policy_command",
            "user_owned_terminal_required",
        ],
    ),
];

/// Strings the scan finds that are not error codes: values, kinds, names and labels that are
/// not in the tool schema. Keep the list sorted.
const NOT_CODES: &[&str] = &[
    "a",
    "allow",
    "app_library",
    "approval_mode",
    "approve",
    "asset_points_at_another_master",
    "calendar",
    "checked",
    "cloudkit_accepted",
    "completed",
    "conflict_check",
    "contacts",
    "date_alarm_moved_to_due_date",
    "date_alarm_removed",
    "deny",
    "destination",
    "drive",
    "exchange",
    "exchange_folder",
    "existing_item",
    "false",
    "file",
    "finished_unconfirmed",
    "folder",
    "full",
    "hidden_or_in_recently_deleted",
    "image",
    "into",
    "jpg",
    "kept_from_apple",
    "library",
    "live_photo",
    "login",
    "logout",
    "meaning",
    "mp4",
    "none",
    "not_in_local_cache",
    "not_shown_by_apple_yet",
    "nothing",
    "number",
    "order",
    "other",
    "outcome",
    "parent_guid",
    "permissions",
    "photo_count",
    "planned_event",
    "policy",
    "priority",
    "quarantine",
    "queued",
    "reads",
    "reads_and_writes",
    "refresh",
    "refused_by_sync_engine",
    "restyled_from_runs",
    "send",
    "send_invitations",
    "services",
    "setup",
    "source",
    "status",
    "tomorrow",
    "true",
    "unchanged",
    "unknown",
    "unlock",
    "verification",
    "video",
    "writes_only",
    "yolo",
];

/// The string literals in `src` that may be returned as error codes: `Err("…")`,
/// `ok_or("…")`, `|_| "…"`, `Refused("…")`, a match arm's `=> "…"`, a `const` and a literal
/// alone on its line. Field and action names from the tool schema are left out.
fn codes() -> BTreeSet<String> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut codes = BTreeSet::new();
    for file in std::fs::read_dir(src).unwrap() {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap();
        for line in text.lines() {
            let trimmed = line.trim();
            for marker in ["Err(\"", "ok_or(\"", "|_| \"", "Refused(\"", "=> \""] {
                let mut rest = line;
                while let Some(at) = rest.find(marker) {
                    rest = &rest[at + marker.len()..];
                    push(&mut codes, rest);
                }
            }
            if trimmed.contains("const ") && trimmed.contains(": &str = \"") {
                if let Some(at) = trimmed.rfind("= \"") {
                    push(&mut codes, &trimmed[at + 3..]);
                }
            }
            if let Some(rest) = trimmed.strip_prefix('"') {
                if rest.trim_end_matches(',').ends_with('"') {
                    push(&mut codes, rest);
                }
            }
        }
    }
    for name in schema_names() {
        codes.remove(&name);
    }
    codes
}

fn push(codes: &mut BTreeSet<String>, rest: &str) {
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    if rest[code.len()..].starts_with('"') && code.starts_with(|c: char| c.is_ascii_lowercase()) {
        codes.insert(code);
    }
}

/// Every tool name, property name and enum value in the server's tool list.
fn schema_names() -> BTreeSet<String> {
    fn walk(value: &Value, names: &mut BTreeSet<String>) {
        match value {
            Value::String(s) => {
                names.insert(s.clone());
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, names)),
            Value::Object(map) => {
                for (key, v) in map {
                    names.insert(key.clone());
                    walk(v, names);
                }
            }
            _ => {}
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = icloud_mcp::Server::open(dir.path()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tools = runtime.block_on(async {
        server.handle(json!({"id":0,"method":"initialize"})).await;
        server
            .handle(json!({"id":1,"method":"tools/list"}))
            .await
            .unwrap()
    });
    let mut names = BTreeSet::new();
    walk(&tools["result"]["tools"], &mut names);
    assert!(names.contains("base_etag"), "{tools}");
    names
}

#[test]
fn every_error_code_has_instructions_or_is_listed_as_needing_none() {
    let dir = tempfile::tempdir().unwrap();
    let codes = codes();
    assert!(codes.len() > 100, "the scan found only {codes:?}");
    let listed: Vec<&str> = NEEDS_NONE
        .iter()
        .flat_map(|(_, c)| c.iter().copied())
        .collect();
    let has_line = |code: &str| icloud_mcp::next_step(code, dir.path(), &Value::Null).is_some();
    let missing: Vec<&String> = codes
        .iter()
        .filter(|code| !has_line(code))
        .filter(|code| !listed.contains(&code.as_str()) && !NOT_CODES.contains(&code.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "give these an instructions line, or list them in NEEDS_NONE or NOT_CODES: {missing:#?}"
    );
    // The terminal group's reason holds: each has its own sentence there.
    let terminal = NEEDS_NONE.last().unwrap();
    assert!(terminal.0.starts_with("Only terminal commands"));
    let bare: Vec<&&str> = terminal
        .1
        .iter()
        .filter(|code| icloud_mcp::terminal::sentence(code, dir.path()).is_none())
        .collect();
    assert!(bare.is_empty(), "give these a terminal sentence: {bare:?}");
    let stale: Vec<&&str> = listed
        .iter()
        .chain(NOT_CODES)
        .filter(|code| !codes.contains(**code) || has_line(code))
        .collect();
    assert!(stale.is_empty(), "remove these from the lists: {stale:?}");
    for (reason, group) in NEEDS_NONE {
        let mut sorted = group.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, *group, "keep the group \"{reason}\" sorted");
    }
    let mut sorted = NOT_CODES.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, NOT_CODES, "keep NOT_CODES sorted");
    let mut once = BTreeSet::new();
    let twice: Vec<&&str> = listed
        .iter()
        .chain(NOT_CODES)
        .filter(|c| !once.insert(**c))
        .collect();
    assert!(twice.is_empty(), "listed twice: {twice:?}");
}

/// The tool errors issue #91 found without a next step. Each now says what to do.
const ISSUE_91: &[&str] = &[
    "folder_not_found",
    "list_unavailable_or_read_only",
    "repeat_requires_due_at",
    "title_required",
    "invalid_date",
    "query_required",
    "absolute_local_path_refused",
    "local_path_traversal_refused",
    "local_hard_link_refused",
    "invalid_name",
    "destination_not_found",
    "invalid_path",
    "root_has_no_details",
];

#[test]
fn the_errors_from_issue_91_say_what_to_do_next() {
    let dir = tempfile::tempdir().unwrap();
    let bare: Vec<&&str> = ISSUE_91
        .iter()
        .filter(|code| {
            icloud_mcp::next_step(code, dir.path(), &Value::Null)
                .is_none_or(|text| text.trim().len() < 20)
        })
        .collect();
    assert!(bare.is_empty(), "give these an instructions line: {bare:?}");
    for code in ISSUE_91 {
        let text = icloud_mcp::next_step(code, dir.path(), &Value::Null).unwrap();
        assert!(!text.to_lowercase().contains("skykeep"), "{code}: {text}");
    }
    let line = |code: &str| icloud_mcp::next_step(code, dir.path(), &Value::Null).unwrap();
    // A second name may be anywhere; the line never tells the AI app to copy past the guard.
    let hard_link = line("local_hard_link_refused");
    assert!(!hard_link.contains("cp "), "{hard_link}");
    assert!(hard_link.contains("not by the AI app"), "{hard_link}");
    // These come back on a live call too, where nothing was queued.
    for code in [
        "preview_too_large",
        "invalid_preview",
        "operation_too_large",
    ] {
        let text = line(code);
        assert!(text.contains("nothing was queued"), "{code}: {text}");
        assert!(text.contains("icloud-mcp quarantine"), "{code}: {text}");
        assert!(text.contains("read the list first"), "{code}: {text}");
    }
    // An older waiting change can fail a new write: nothing is queued or set aside then.
    for code in ["payload_too_large", "unknown_service"] {
        let text = line(code);
        assert!(
            text.contains("nothing was queued or set aside by this call"),
            "{code}: {text}"
        );
        assert!(text.contains("icloud-mcp quarantine"), "{code}: {text}");
        assert!(text.contains("read the list first"), "{code}: {text}");
        assert!(!text.contains("smaller change"), "{code}: {text}");
        assert!(!text.contains("It was set aside"), "{code}: {text}");
    }
    // A live note create or a large cached item fails the new write itself: no older
    // change is to blame, and the note can still be made in Apple Notes.
    for code in ["template_too_large", "operation_too_large"] {
        let text = line(code);
        assert!(text.contains("nothing was queued"), "{code}: {text}");
        assert!(text.contains("in Apple Notes"), "{code}: {text}");
        assert!(text.contains("icloud-mcp quarantine"), "{code}: {text}");
        assert!(text.contains("read the list first"), "{code}: {text}");
        assert!(!text.contains("older change"), "{code}: {text}");
    }
    let path = line("invalid_path");
    assert!(path.contains("255 bytes"), "{path}");
}
