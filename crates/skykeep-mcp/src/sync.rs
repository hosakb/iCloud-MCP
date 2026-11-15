//! Only Notes and Reminders run here. Reuse upstream passes and queue/crash semantics.
use quince_auth::{Client, Endpoints, Store};
use quince_core::Library;
use quince_sync::{library_lock::LibraryLock, pass, queue};
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::Mutex;

pub(crate) const MAX_PENDING: usize = 128;
/// Refused rows kept for the human before admission stops.
pub const MAX_REFUSED: usize = 1024;

pub(crate) fn pending_operations(
    library: &Library,
) -> Result<Vec<quince_core::outbox::Pending>, &'static str> {
    // Refused rows wait for the human (`skykeep-mcp quarantine`); they never count here.
    let all = library.outbox_count().map_err(|_| "library_unavailable")?;
    let refused = library
        .outbox_refused_count()
        .map_err(|_| "library_unavailable")?;
    if all.saturating_sub(refused) > MAX_PENDING {
        return Err("queue_too_large");
    }
    let mut pending = library
        .outbox_pending("notes")
        .map_err(|_| "library_unavailable")?;
    pending.extend(
        library
            .outbox_pending("reminders")
            .map_err(|_| "library_unavailable")?,
    );
    if pending.iter().any(|p| p.payload.len() > 262144) {
        return Err("payload_too_large");
    }
    Ok(pending)
}

fn pending_value(p: &quince_core::outbox::Pending) -> Value {
    // Tries changes on every retry. All operation-defining bytes must stay exact.
    json!({"id":p.id,"service":p.service,"record":p.record,"kind":p.kind,"payload":p.payload})
}

fn send_context(
    library: &Library,
    p: &quince_core::outbox::Pending,
) -> Result<Value, &'static str> {
    match (p.service.as_str(), p.kind.as_str()) {
        ("notes", "create") => {
            let template = library
                .state(quince_icloud::notes::TEMPLATE_KEY)
                .map_err(|_| "library_unavailable")?;
            if template.as_ref().is_some_and(|t| t.len() > 131072) {
                return Err("template_too_large");
            }
            Ok(json!({"template":template}))
        }
        ("reminders", "create") | (_, "delete") => Ok(Value::Null),
        // Only the cached rows the native sender reads for this operation. Lists, folders
        // and sibling items stay out, so unrelated refreshes and writes cannot stale it.
        ("notes", kind) => {
            let mut item = library
                .note(&p.record)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            if kind == "save" {
                item.body = library
                    .note_body(&p.record)
                    .map_err(|_| "library_unavailable")?;
                item.runs = library
                    .note_runs(&p.record)
                    .map_err(|_| "library_unavailable")?;
            }
            bounded_context(json!({"item":crate::bounded_json(&item)?}))
        }
        ("reminders", kind) => {
            let item = library
                .reminder(&p.record)
                .map_err(|_| "library_unavailable")?
                .ok_or("not_found")?;
            // An edit that moves lists also moves the cached subtasks with it.
            let edit: Value = serde_json::from_str(&p.payload).unwrap_or(Value::Null);
            let moves = kind == "edit"
                && edit["list_guid"]
                    .as_str()
                    .is_some_and(|to| Some(to) != edit["from_list"].as_str());
            let children = if moves {
                crate::bounded_json(
                    &library
                        .subtasks_of(&p.record)
                        .map_err(|_| "library_unavailable")?,
                )?
            } else {
                Value::Null
            };
            bounded_context(json!({"item":crate::bounded_json(&item)?,"children":children}))
        }
        _ => Err("unknown_service"),
    }
}

fn bounded_context(context: Value) -> Result<Value, &'static str> {
    crate::bounded_value(&context, crate::MAX_ARGUMENT_BYTES)?;
    Ok(context)
}

/// Grant failures that belong to one queued write. Anything else (an unreadable library,
/// a changed policy, account or directory) still stops the whole pass.
const QUARANTINABLE: [&str; 9] = [
    "payload_too_large",
    "legacy_write_quarantined",
    "authorization_too_large",
    "invalid_authorization",
    "write_authorization_stale",
    "not_found",
    "operation_too_large",
    "template_too_large",
    "unknown_service",
];

/// Moves each pending write without a current grant out of the queue, alone, with its
/// reason. It is kept for the human and never sent or re-authorized.
fn quarantine_unauthorized(library: &mut Library, dir: &Path) -> Result<(), &'static str> {
    let pending = library
        .outbox_all_pending()
        .map_err(|_| "library_unavailable")?;
    for p in pending {
        match authorized(library, dir, &p) {
            Ok(()) => {}
            Err(reason) if QUARANTINABLE.contains(&reason) => library
                .atomically(|library| -> quince_core::Result<()> {
                    library.outbox_refuse(p.id, &format!("mcp_quarantine:{reason}"))?;
                    library.set_state(&format!("mcp.authorization.{}", p.id), "")
                })
                .map_err(|_| "library_unavailable")?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn record_authorization(
    library: &mut Library,
    p: &quince_core::outbox::Pending,
) -> Result<(), &'static str> {
    let policy = library
        .state("mcp.policy")
        .map_err(|_| "policy_unavailable")?
        .ok_or("policy_unavailable")?;
    let grant =
        json!({"policy":policy,"pending":pending_value(p),"context":send_context(library,p)?});
    library
        .set_state(&format!("mcp.authorization.{}", p.id), &grant.to_string())
        .map_err(|_| "library_unavailable")
}

/// Testable send boundary. No tool can create or change this authorization.
pub fn authorized(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
) -> Result<(), &'static str> {
    authorization_check(library, dir, p, true)
}

pub(crate) fn authorized_payload(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
) -> Result<(), &'static str> {
    authorization_check(library, dir, p, false)
}

fn authorization_check(
    library: &Library,
    dir: &Path,
    p: &quince_core::outbox::Pending,
    context: bool,
) -> Result<(), &'static str> {
    let Some((_, binding)) = crate::policy(library, dir)? else {
        return Err("configured_policy_required");
    };
    if p.payload.len() > 262144 {
        return Err("payload_too_large");
    }
    let raw = library
        .state(&format!("mcp.authorization.{}", p.id))
        .map_err(|_| "library_unavailable")?
        .ok_or("legacy_write_quarantined")?;
    if raw.len() > 1572864 {
        return Err("authorization_too_large");
    }
    let grant: Value = serde_json::from_str(&raw).map_err(|_| "invalid_authorization")?;
    if grant.as_object().is_none_or(|m| m.len() != 3)
        || grant["policy"] != binding.policy
        || grant["pending"] != pending_value(p)
        || (context && grant["context"] != send_context(library, p)?)
    {
        return Err("write_authorization_stale");
    }
    Ok(())
}

pub async fn run(dir: &Path) -> Result<Value, &'static str> {
    run_authorized(dir, false).await
}

fn refresh_gate(dir: &Path, approved: bool) -> Result<bool, &'static str> {
    let mut library = Library::open(dir).map_err(|_| "library_unavailable")?;
    let configured = crate::policy(&library, dir)?;
    if configured
        .as_ref()
        .is_some_and(|(p, _)| p.mode == "reads_and_writes")
        && !approved
    {
        return Err("read_approval_required");
    }
    if configured.is_some() {
        quarantine_unauthorized(&mut library, dir)?;
    }
    Ok(configured.is_some())
}

pub(crate) async fn run_authorized(dir: &Path, approved: bool) -> Result<Value, &'static str> {
    refresh_gate(dir, approved)?;
    let store = Store::isolated_keyring(dir).map_err(|_| "secure_storage_unavailable")?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    let client = Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")?;
    with_client_inner(dir, client, approved).await
}

/// Test seam with synthetic endpoints. Production constructs its own Apple-only client.
/// Caller holds the private directory's exclusive operation lock.
pub async fn with_client(dir: &Path, client: Client) -> Result<Value, &'static str> {
    with_client_inner(dir, client, false).await
}

async fn with_client_inner(
    dir: &Path,
    client: Client,
    approved: bool,
) -> Result<Value, &'static str> {
    let configured = refresh_gate(dir, approved)?;
    if configured {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            sync_pass(dir, client, true),
        )
        .await
        .map_err(|_| "sync_timeout_delivery_uncertain")?
    } else {
        sync_pass(dir, client, false).await
    }
}

async fn sync_pass(
    dir: &Path,
    mut client: Client,
    configured: bool,
) -> Result<Value, &'static str> {
    if !client
        .resume()
        .await
        .map_err(|_| "login_required_or_network_unavailable")?
    {
        return Err("login_required");
    }
    if client.waits_on_icloud_com() {
        return Err("login_required_complete_account_setup");
    }
    let dsid = client.session().dsid.as_deref().ok_or("login_required")?;
    let tag = queue::account_tag(dsid);
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    if crate::account::binding(&library, dsid)? != crate::account::Binding::Same {
        return Err("account_mismatch_use_new_state_directory");
    }
    let data = LibraryLock::new(library, Some(dir.to_owned()));
    let ctx = pass::Ctx::new(
        client.http().clone(),
        client.service_params(),
        client.session().service_url("ckdatabasews"),
        client.data_access_missing(),
        &pass::NoHooks,
    );
    let root = ctx.cloudkit.as_deref().ok_or("service_unavailable")?;
    let mut services = Vec::new();
    for service in ["notes", "reminders"] {
        let lock = Mutex::new(());
        let before = pass::run(service, &ctx, &data, &lock).await;
        // Never send when the authenticated read is gated or failed.
        let mut sent = false;
        if matches!(before.outcome, pass::Outcome::Ok) {
            let report = queue::drain_as(&data, service, &lock, &tag, |p| {
                let data = &data;
                let http = &ctx.http;
                let params = ctx.params.clone();
                async move {
                    if configured {
                        let library = data.lock().await;
                        authorized(&library, dir, &p).map_err(|_| {
                            quince_icloud::Error::Refused(
                                "Skykeep write authorization is missing or stale; quarantined"
                                    .into(),
                            )
                        })?;
                    }
                    if service == "notes" {
                        quince_sync::notes::send(data, http, root, params, p).await
                    } else {
                        quince_sync::reminders::send(data, http, root, params, p).await
                    }
                }
            })
            .await;
            sent = report.sent;
        }
        let after = if sent {
            pass::run(service, &ctx, &data, &lock).await
        } else {
            before
        };
        // Raw upstream error details can contain HTTP bodies. Expose only typed outcomes.
        services.push(json!({"service":service,"outcome":after.outcome,"sent":sent,"last_synced_at":after.last_synced_at}));
    }
    let library = data.lock().await;
    Ok(
        json!({"services":services,"queued":library.outbox_count().map_err(|_| "library_unavailable")?,"remote_verified":false,"verification":"upstream sync and replay only; field-level read-back is not guaranteed"}),
    )
}
