//! Every error code in the server's source either has an `instructions` line or is listed
//! below as needing none. A new code fails this test until it is given one or listed.
use serde_json::Value;
use std::collections::BTreeSet;

/// Codes whose name already says what happened and what to do, or that need no step from
/// the person. `unsafe_state_contents` has a line only when the folder holds such an entry.
/// Keep the list sorted.
const NEEDS_NONE: &[&str] = &[
    "absolute_local_path_refused",
    "account_changed_during_refresh_cache_kept",
    "account_mismatch",
    "account_mismatch_use_new_state_directory",
    "album_is_folder",
    "all_day_cannot_have_timed_alert",
    "all_day_requires_due_at",
    "already_initialized",
    "ambiguous_path_use_id",
    "app_library_cannot_be_changed",
    "approval_cancelled",
    "approval_denied",
    "approval_not_found",
    "approval_pending",
    "approval_policy_changed",
    "approval_refused",
    "approval_required",
    "approval_required_or_stale",
    "approval_requires_form_client",
    "approval_stale_calendar_changed_ask_again",
    "approval_stale_drive_changed_ask_again",
    "approval_timeout",
    "authorization_too_large",
    "calendar_guid_required",
    "calendar_guid_too_long",
    "calendar_paused_approve_on_apple_device",
    "calendar_read_failed_nothing_sent",
    "calendar_refresh_failed_cache_kept",
    "calendar_refresh_timeout_cache_kept",
    "calendar_response_too_large_cache_kept",
    "calendar_timed_out_nothing_sent",
    "calendar_too_many_events_cache_kept",
    "calendar_write_not_confirmed_run_calendar_refresh",
    "cannot_move_a_folder_into_itself",
    "clock_unavailable",
    "configured_policy_required",
    "conflict",
    "contact_delete_needs_only_guid_and_etag",
    "contact_too_large",
    "contact_unknown_field",
    "contacts_book_too_large_cache_kept",
    "contacts_card_too_large_cache_kept",
    "contacts_paused_approve_on_apple_device",
    "contacts_refresh_failed_cache_kept",
    "contacts_refresh_timeout_cache_kept",
    "contacts_response_too_large_cache_kept",
    "denied_by_permissions",
    "destination_not_found",
    "drive_ambiguous_path_cache_kept",
    "drive_download_failed_nothing_kept",
    "drive_download_incomplete_nothing_kept",
    "drive_download_timed_out_nothing_kept",
    "drive_file_over_exchange_limit",
    "drive_folder_changed_not_trashed_run_drive_refresh",
    "drive_folder_in_trash_cache_kept",
    "drive_folder_left_cache_cache_kept",
    "drive_folder_not_found_cache_kept",
    "drive_folder_too_large_cache_kept",
    "drive_item_changed_run_drive_refresh",
    "drive_item_too_large_cache_kept",
    "drive_name_not_usable_locally_give_local",
    "drive_not_a_folder_cache_kept",
    "drive_path_not_found_cache_kept",
    "drive_paused_approve_on_apple_device",
    "drive_refresh_failed_cache_kept",
    "drive_refresh_timeout_cache_kept",
    "drive_response_too_large_cache_kept",
    "drive_write_not_confirmed_run_drive_refresh",
    "due_time_alert_requires_due_at",
    "early_alert_account_unavailable_set_one_on_an_apple_device_then_sync",
    "early_alert_requires_due_at",
    "event_too_large",
    "exchange_limit_out_of_range",
    "exchange_parent_missing",
    "folder_not_found",
    "folder_not_in_cache_use_path",
    "folder_not_writable",
    "foreground_terminal_required",
    "frame_too_large",
    "guid_required",
    "home_folder_unknown_give_path",
    "input_cancelled",
    "input_timeout",
    "interactive_terminal_required",
    "invalid_arguments",
    "invalid_arguments_move_changes_only_the_calendar",
    "invalid_arguments_use_move_for_another_calendar",
    "invalid_authorization",
    "invalid_cached_runs",
    "invalid_cached_save",
    "invalid_date",
    "invalid_early_alert",
    "invalid_exchange_setting",
    "invalid_id",
    "invalid_input",
    "invalid_local_path",
    "invalid_name",
    "invalid_patch",
    "invalid_path",
    "invalid_policy",
    "invalid_policy_mode",
    "invalid_preview",
    "invalid_range",
    "invalid_repeat_end",
    "invalid_request_id",
    "invalid_tool",
    "invalid_utf8",
    "item_too_large",
    "legacy_approval_quarantined",
    "legacy_write_quarantined",
    "library_unavailable",
    "list_unavailable_or_read_only",
    "local_file_changed_while_read",
    "local_file_unavailable",
    "local_hard_link_refused",
    "local_not_a_regular_file",
    "local_path_too_deep",
    "local_path_too_long",
    "local_path_traversal_refused",
    "local_write_failed",
    "login_incomplete",
    "login_required_complete_account_setup",
    "login_required_or_network_unavailable",
    "login_required_unlock_secure_storage",
    "logout_pending_unlock_keyring_and_retry",
    "lossy_markdown_overwrite",
    "not_a_file",
    "not_a_folder",
    "not_found",
    "not_found_at_apple_run_calendar_refresh",
    "not_found_at_apple_run_drive_refresh",
    "note_body_unavailable_sync_first",
    "operation_too_large",
    "page_too_large",
    "parent_not_found",
    "parent_required",
    "path_too_deep",
    "path_too_long",
    "payload_too_large",
    "pending_changed",
    "permissions_file_unavailable",
    "permissions_log_full",
    "permissions_log_unavailable",
    "permissions_setup_required",
    "photo_too_large",
    "photos_bad_continuation_cache_kept",
    "photos_library_preparing_cache_kept",
    "photos_library_unavailable_cache_kept",
    "photos_refresh_timeout_cache_kept",
    "photos_response_too_large_cache_kept",
    "policy_change_cancelled",
    "policy_mode_required",
    "policy_unavailable",
    "preview_too_large",
    "quarantine_full_retire_in_terminal",
    "quarantine_retire_cancelled",
    "query_required",
    "query_too_long",
    "queue_too_large",
    "read_approval_required",
    "read_only_note",
    "repeat_end_before_due",
    "repeat_requires_due_at",
    "repeating_reminder_complete_with_tick",
    "repeating_reminder_needs_due_date",
    "root_has_no_details",
    "runs_do_not_match_body",
    "search_unavailable",
    "secure_storage_unavailable",
    "security_key_sign_in_unsupported",
    "service_unavailable",
    "staging_unavailable",
    "state_busy_retry",
    "state_directory_required",
    "state_must_be_owned_and_private",
    "state_unavailable",
    "storage_already_chosen",
    "sync_timeout_delivery_uncertain",
    "template_too_large",
    "terminal_unavailable",
    "timezone_database_unavailable",
    "timezone_requires_timed_due_at",
    "title_required",
    "too_many_code_requests_try_later",
    "top_folder_cannot_be_changed",
    "transport_failed",
    "trash_not_supported",
    "unexpected_argument",
    "unknown_service",
    "unknown_tool",
    "unsafe_state_contents",
    "unsafe_state_path",
    "unsupported_attachment_insertion",
    "unsupported_attachment_move",
    "unsupported_attachment_removal",
    "use_policy_command",
    "user_owned_terminal_required",
    "write_authorization_stale",
    "write_refused",
];

/// The string literals in `src` that are returned as error codes: `Err("…")`, `ok_or("…")`,
/// `|_| "…"`, a `Stop` variant's code and a `const` code.
fn codes() -> BTreeSet<String> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut codes = BTreeSet::new();
    for file in std::fs::read_dir(src).unwrap() {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap();
        for line in text.lines() {
            let trimmed = line.trim_start();
            let constant = trimmed.contains("const ") && trimmed.contains(": &str = \"");
            let stop = trimmed.starts_with("Stop::") && trimmed.contains("=> \"");
            for marker in ["Err(\"", "ok_or(\"", "|_| \""] {
                let mut rest = line;
                while let Some(at) = rest.find(marker) {
                    rest = &rest[at + marker.len()..];
                    push(&mut codes, rest);
                }
            }
            if constant || stop {
                if let Some(at) = line.rfind("= \"").or_else(|| line.rfind("> \"")) {
                    push(&mut codes, &line[at + 3..]);
                }
            }
        }
    }
    // Names in `const` lines that are not error codes.
    for name in [
        "drive",
        "contacts",
        "calendar",
        "drive_write",
        "contacts_write",
        "calendar_write",
        "photos_write",
        "send_invitations",
    ] {
        codes.remove(name);
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

#[test]
fn every_error_code_has_instructions_or_is_listed_as_needing_none() {
    let dir = tempfile::tempdir().unwrap();
    let codes = codes();
    assert!(codes.len() > 100, "the scan found only {codes:?}");
    let missing: Vec<&String> = codes
        .iter()
        .filter(|code| skykeep_mcp::next_step(code, dir.path(), &Value::Null).is_none())
        .filter(|code| !NEEDS_NONE.contains(&code.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "give these an instructions line or list them in NEEDS_NONE: {missing:#?}"
    );
    let stale: Vec<&&str> = NEEDS_NONE
        .iter()
        .filter(|code| {
            !codes.contains(**code)
                || skykeep_mcp::next_step(code, dir.path(), &Value::Null).is_some()
        })
        .collect();
    assert!(stale.is_empty(), "remove these from NEEDS_NONE: {stale:?}");
    let mut sorted = NEEDS_NONE.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, NEEDS_NONE, "keep NEEDS_NONE sorted");
}
