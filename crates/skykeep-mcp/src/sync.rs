//! Only Notes and Reminders run here. Reuse upstream passes and queue/crash semantics.
use quince_auth::{Client, Endpoints, Store};
use quince_core::Library;
use quince_sync::{library_lock::LibraryLock, pass, queue};
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::Mutex;

pub async fn run(dir: &Path) -> Result<Value, &'static str> {
    let store = Store::isolated_keyring(dir).map_err(|_| "secure_storage_unavailable")?;
    let session = store
        .try_load()
        .map_err(|_| "login_required_unlock_secure_storage")?;
    if !session.is_resumable() {
        return Err("login_required");
    }
    let client = Client::with(dir, Endpoints::default(), store).map_err(|_| "login_required")?;
    with_client(dir, client).await
}

/// Test seam with synthetic endpoints. Production constructs its own Apple-only client.
/// Caller holds the private directory's exclusive operation lock.
pub async fn with_client(dir: &Path, mut client: Client) -> Result<Value, &'static str> {
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
