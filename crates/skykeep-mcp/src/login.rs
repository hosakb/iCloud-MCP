//! Human-owned authentication. Nothing in this module is callable through MCP.
use quince_auth::{Client, Error};
use zeroize::Zeroizing;

enum CodeRoute {
    TrustedDevice,
    Sms(u64),
}

pub fn auth_error(error: Error) -> &'static str {
    match error {
        Error::BadCredentials => "credentials_rejected",
        Error::BadCode => "code_incorrect_or_expired",
        Error::CoolingDown { .. } | Error::RateLimited => "auth_cooldown_try_later",
        Error::AccountLocked => "account_locked_visit_account_website",
        Error::Keyring(_) => "secure_storage_unavailable",
        Error::CodeAgain | Error::Moved => "regional_login_retry_required",
        _ => "authentication_unavailable",
    }
}

pub async fn authenticate<F>(client: &mut Client, prompt: &mut F) -> Result<(), &'static str>
where
    F: FnMut(&str) -> Result<Zeroizing<String>, &'static str>,
{
    let account = prompt("Apple Account (hidden): ")?;
    if client.session().apple_id.as_deref().is_some_and(|old| {
        quince_auth::session::account_fingerprint(old)
            != quince_auth::session::account_fingerprint(&account)
    }) {
        return Err("account_mismatch_use_new_state_directory");
    }
    let password = prompt("Apple Account password (hidden, Ctrl-C cancels): ")?;
    let outcome = client
        .sign_in(account.trim(), &password)
        .await
        .map_err(auth_error)?;
    if outcome == quince_auth::SignInOutcome::NeedsTwoFactor {
        let options = client.two_factor_options().await.map_err(auth_error)?;
        let devices = options.trusted_device_count > 0;
        let phones: Vec<_> = options.phones.iter().map(|p| p.id).collect();
        if !devices && phones.is_empty() {
            return Err("no_available_code_routes");
        }
        let label = format!(
            "{}Code route: {}{}phone IDs {:?} (hidden, Ctrl-C cancels): ",
            if devices {
                ""
            } else {
                "Trusted device unavailable. "
            },
            if devices {
                "0 for trusted device; "
            } else {
                ""
            },
            if phones.contains(&0) {
                "sms:0 for SMS phone ID 0; "
            } else {
                ""
            },
            phones
        );
        // A mistyped or unavailable route must not discard the completed credential step.
        // Prompt errors still propagate immediately, including cancellation and timeout.
        let mut retry = false;
        let route = loop {
            let answer = prompt(&if retry {
                format!("Invalid or unavailable code route. {label}")
            } else {
                label.clone()
            })?;
            if answer.as_str() == "sms:0" && phones.contains(&0) {
                break CodeRoute::Sms(0);
            }
            if let Ok(id) = answer.parse::<u64>() {
                if id == 0 && devices {
                    break CodeRoute::TrustedDevice;
                }
                if id != 0 && phones.contains(&id) {
                    break CodeRoute::Sms(id);
                }
            }
            retry = true;
        };
        match route {
            CodeRoute::TrustedDevice => client.request_code().await,
            CodeRoute::Sms(id) => client.send_sms(id).await,
        }
        .map_err(auth_error)?;
        for attempt in 0..3 {
            let code = prompt(if attempt == 0 {
                "Apple verification code (hidden): "
            } else {
                "Code incorrect or expired. New code (hidden): "
            })?;
            let result = match route {
                CodeRoute::TrustedDevice => client.submit_two_factor(&code).await,
                CodeRoute::Sms(id) => client.submit_sms_code(id, &code).await,
            };
            match result {
                Ok(()) => break,
                Err(Error::BadCode) if attempt < 2 => continue,
                Err(e) => return Err(auth_error(e)),
            }
        }
    }
    if client.unsaved().is_some() {
        return Err("secure_storage_unavailable");
    }
    if !client.session().is_resumable() || client.session().dsid.is_none() {
        return Err("login_incomplete");
    }
    Ok(())
}

/// Ties the private directory to the signed-in account, so the MCP server's sync resumes this
/// session only for the account whose data the directory holds. A login for a different
/// account is signed out again rather than left behind to resume.
/// Caller holds the private directory's exclusive operation lock.
pub fn bind_account(dir: &std::path::Path, client: &mut Client) -> Result<(), &'static str> {
    if !client.session().is_resumable() {
        return Err("login_incomplete");
    }
    let dsid = client.session().dsid.as_deref().ok_or("login_incomplete")?;
    let mut library = quince_core::Library::open(dir).map_err(|_| "library_unavailable")?;
    match crate::account::binding(&library, dsid)? {
        crate::account::Binding::Same => Ok(()),
        crate::account::Binding::Unbound(tag) => library
            .set_state(quince_sync::queue::ACCOUNT_KEY, &tag)
            .map_err(|_| "library_unavailable"),
        crate::account::Binding::Other => {
            let _ = client.sign_out();
            Err("account_mismatch_use_new_state_directory")
        }
    }
}
