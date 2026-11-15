//! Only Notes and Reminders run here. Reuse upstream passes and queue/crash semantics.
use quince_auth::{Client, Endpoints, Store};
use quince_core::Library;
use quince_sync::{library_lock::LibraryLock, pass, queue};
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::Mutex;

pub(crate) const MAX_PENDING: usize = 128;

pub(crate) fn pending_operations(
    library: &Library,
) -> Result<Vec<quince_core::outbox::Pending>, &'static str> {
    if library.outbox_count().map_err(|_| "library_unavailable")? > MAX_PENDING {
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
        ("notes", _) => crate::resolved_state(library, "notes_write", &p.record),
        ("reminders", _) => crate::resolved_state(library, "reminders_write", &p.record),
        _ => Err("unknown_service"),
    }
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
    let library = Library::open(dir).map_err(|_| "library_unavailable")?;
    let configured = crate::policy(&library, dir)?;
    if configured
        .as_ref()
        .is_some_and(|(p, _)| p.mode == "reads_and_writes")
        && !approved
    {
        return Err("read_approval_required");
    }
    if configured.is_some() {
        for p in pending_operations(&library)? {
            authorized(&library, dir, &p)?;
        }
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
