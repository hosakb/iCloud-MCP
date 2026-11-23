//! Human-owned authentication. Nothing in this module is callable through MCP.
use quince_auth::{Client, Error};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeRoute {
    TrustedDevice,
    Phone(u64),
}

/// Whether a prompt's answer may be shown as it is typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    /// Passwords, codes, accounts: never shown.
    Hidden,
    /// A menu choice.
    Shown,
}

/// The most codes one sign-in asks Apple to send. Going back to choose another way sends a new
/// one, and asking for many in a row is what makes Apple refuse further sends for a while.
const MOST_SENDS: usize = 4;

/// What Apple says about a number, made safe to print: no control or text-direction characters
/// that could rearrange the terminal, and no more than a phone number needs.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .take(40)
        .collect::<String>()
        .trim()
        .to_string()
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
    authenticate_with(client, &mut |label, _| prompt(label)).await
}

/// [`authenticate`], told which prompts are a menu choice that may be shown as it is typed.
pub async fn authenticate_with<F>(client: &mut Client, prompt: &mut F) -> Result<(), &'static str>
where
    F: FnMut(&str, Echo) -> Result<Zeroizing<String>, &'static str>,
{
    let account = prompt("Apple Account (hidden): ", Echo::Hidden)?;
    if client.session().apple_id.as_deref().is_some_and(|old| {
        quince_auth::session::account_fingerprint(old)
            != quince_auth::session::account_fingerprint(&account)
    }) {
        return Err("account_mismatch_use_new_state_directory");
    }
    let password = prompt(
        "Apple Account password (hidden, Ctrl-C cancels): ",
        Echo::Hidden,
    )?;
    let outcome = client
        .sign_in(account.trim(), &password)
        .await
        .map_err(auth_error)?;
    drop(password);
    if outcome == quince_auth::SignInOutcome::NeedsTwoFactor {
        let options = client.two_factor_options().await.map_err(auth_error)?;
        // Apple's reported count is not a reliable gate: a code has arrived on a trusted device
        // while this answer reported zero. Like the desktop app's code screen, the trusted-device
        // route is always offered; a zero count only adds a note.
        let devices = options.trusted_device_count > 0;
        // A security-key-only answer offers no code at all (the desktop app shows its
        // security-key screen instead); Skykeep cannot answer the key challenge.
        if options.security_key && !devices && options.phones.is_empty() {
            return Err("security_key_sign_in_unsupported");
        }
        // Apple names no devices, so the device choice says only "my Apple devices". A number
        // is shown as Apple masked it, and only when Apple gave one.
        let mut routes = vec![(
            "Send the code to my Apple devices".to_string(),
            CodeRoute::TrustedDevice,
        )];
        for (n, phone) in options.phones.iter().enumerate() {
            // quince-auth writes "(hidden)" when Apple sent no number at all.
            let number = printable(&phone.masked_number);
            let number = if number.is_empty() || number == "(hidden)" {
                format!("trusted phone number {}", n + 1)
            } else {
                number
            };
            let text = if phone.mode == "voice" {
                format!("Call me at {number}")
            } else {
                format!("Send a text message to {number}")
            };
            routes.push((text, CodeRoute::Phone(phone.id)));
        }
        let mut menu = String::from("Where should Apple send the verification code?\n");
        if !devices {
            menu.push_str(
                "Apple reported no trusted devices, but a code may still arrive on them.\n",
            );
        }
        if options.too_many_codes_sent {
            menu.push_str(
                "Apple says it sent many codes recently and may refuse new ones for a while.\n",
            );
        }
        for (i, (text, _)) in routes.iter().enumerate() {
            menu.push_str(&format!("  {}. {text}\n", i + 1));
        }
        menu.push_str("Choose a number: ");
        let mut sends = 0;
        // Choosing a way and typing the code are separate steps. Going back from the code to
        // the choice, or mistyping the choice, never repeats the password step. Prompt errors
        // still propagate at once, including cancellation and timeout.
        'choose: loop {
            let mut retry = false;
            let route = loop {
                let answer = prompt(
                    &if retry {
                        format!("That is not one of the choices.\n{menu}")
                    } else {
                        menu.clone()
                    },
                    Echo::Shown,
                )?;
                match answer.trim().parse::<usize>() {
                    Ok(n) if (1..=routes.len()).contains(&n) => break routes[n - 1].1,
                    _ => retry = true,
                }
            };
            if sends == MOST_SENDS {
                return Err("too_many_code_requests_try_later");
            }
            sends += 1;
            match route {
                // The first time, the push sign-in may already have made counts; coming back
                // to the devices asks Apple for a fresh one.
                CodeRoute::TrustedDevice if sends == 1 => client.request_code().await,
                CodeRoute::TrustedDevice => client.resend_to_devices().await,
                CodeRoute::Phone(id) => client.send_sms(id).await,
            }
            .map_err(auth_error)?;
            let back = if sends < MOST_SENDS {
                "; type b to choose another way"
            } else {
                ""
            };
            for attempt in 0..3 {
                let code = prompt(
                    &if attempt == 0 {
                        format!("Apple verification code (hidden{back}): ")
                    } else {
                        format!("That code didn't match or has expired. Type the new code (hidden{back}): ")
                    },
                    Echo::Hidden,
                )?;
                if !back.is_empty() && code.trim().eq_ignore_ascii_case("b") {
                    continue 'choose;
                }
                let result = match route {
                    CodeRoute::TrustedDevice => client.submit_two_factor(&code).await,
                    CodeRoute::Phone(id) => client.submit_sms_code(id, &code).await,
                };
                match result {
                    Ok(()) => break 'choose,
                    Err(Error::BadCode) if attempt < 2 => continue,
                    Err(e) => return Err(auth_error(e)),
                }
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
        crate::account::Binding::Same => {}
        crate::account::Binding::Unbound(tag) => library
            .set_state(quince_sync::queue::ACCOUNT_KEY, &tag)
            .map_err(|_| "library_unavailable")?,
        crate::account::Binding::Other => {
            let _ = client.sign_out();
            return Err("account_mismatch_use_new_state_directory");
        }
    }
    // A new sign-in: an earlier failed check or sign-out no longer speaks for it.
    crate::forget_session_check(&mut library);
    Ok(())
}
