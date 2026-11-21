//! Terminal login, through the hidden prompt on a test-owned PTY, then the MCP sync path
//! reopening that saved session from a fresh client. Loopback fixtures, fictional values only.
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use quince_auth::{Client, Endpoints, Store};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::time::Duration;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRETS: [&str; 3] = [
    "fixture@example.invalid",
    "SYNTHETIC-PASSWORD-SENTINEL",
    "424242",
];

fn pair() -> (File, File) {
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

async fn apple(server: &MockServer) {
    let ck = json!({"ckdatabasews":{"url":server.uri()}});
    for (verb, endpoint, response) in [
        ("POST", "/appleauth/auth/signin/init", ResponseTemplate::new(200).set_body_json(json!({"salt":B64.encode([0x77;16]),"b":B64.encode([0x5e;256]),"c":"challenge","iteration":10,"protocol":"s2k"}))),
        ("POST", "/appleauth/auth/signin/complete", ResponseTemplate::new(409).insert_header("X-Apple-Session-Token","SYNTHETIC-PENDING-TOKEN").set_body_json(json!({}))),
        ("PUT", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth", ResponseTemplate::new(200).set_body_json(json!({"trustedDeviceCount":1,"trustedPhoneNumbers":[]}))),
        ("POST", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth/2sv/trust", ResponseTemplate::new(204).insert_header("X-Apple-TwoSV-Trust-Token","SYNTHETIC-TRUST").insert_header("X-Apple-Session-Token","SYNTHETIC-COMPLETE-TOKEN")),
        ("POST", "/setup/ws/1/accountLogin", ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":ck}))),
        ("POST", "/setup/ws/1/validate", ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":ck}))),
    ] { Mock::given(method(verb)).and(path(endpoint)).respond_with(response).mount(server).await; }
    Mock::given(method("POST"))
        .and(path_regex(".*/changes/zone$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"zones":[{"records":[],"syncToken":"synthetic-cursor","moreComing":false}]}),
        ))
        .mount(server)
        .await;
}

/// Types each answer only after its whole prompt is shown, and returns everything the
/// terminal displayed.
fn typist(mut master: File) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut transcript = Vec::new();
        for answer in [SECRETS[0], SECRETS[1], "1", SECRETS[2]] {
            loop {
                let mut buf = [0; 512];
                let n = master.read(&mut buf).unwrap();
                transcript.extend_from_slice(&buf[..n]);
                if transcript.ends_with(b": ") {
                    break;
                }
            }
            master.write_all(answer.as_bytes()).unwrap();
            master.write_all(b"\r").unwrap();
        }
        let mut buf = [0; 512];
        let n = master.read(&mut buf).unwrap_or(0);
        transcript.extend_from_slice(&buf[..n]);
        String::from_utf8_lossy(&transcript).into_owned()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_login_session_is_reused_by_a_fresh_mcp_sync() {
    let server = MockServer::start().await;
    apple(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let (master, mut slave) = pair();
    let typed = typist(master);

    // The login helper's process.
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    skykeep_mcp::login::authenticate(&mut client, &mut |label| {
        skykeep_mcp::terminal::hidden(&mut slave, label, Duration::from_secs(5))
    })
    .await
    .unwrap();
    skykeep_mcp::login::bind_account(dir.path(), &mut client).unwrap();
    drop(client);

    let transcript = typed.join().unwrap();
    let password = transcript.find("password (hidden").expect(&transcript);
    let code = transcript
        .find("verification code (hidden")
        .expect(&transcript);
    assert!(password < code, "password must be asked before the code");
    for secret in SECRETS {
        assert!(!transcript.contains(secret), "terminal echoed {secret}");
    }

    // The MCP server's process: nothing carried over but the saved store.
    let store = Store::file(dir.path()).unwrap().without_passwords();
    let saved = store.try_load().unwrap();
    assert!(saved.is_resumable());
    assert!(saved.password.is_none());
    let reopened = Client::with(dir.path(), Endpoints::at(&server.uri()), store).unwrap();
    let result = skykeep_mcp::sync::with_client(dir.path(), reopened)
        .await
        .expect("saved terminal session should resume for MCP sync");
    assert_eq!(result["services"].as_array().unwrap().len(), 2);

    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .any(|r| r.url.path() == "/setup/ws/1/validate"));
    assert!(requests
        .iter()
        .any(|r| r.url.path().contains("com.apple.notes")));

    // Nothing the person typed is in the MCP result or anywhere in the private directory.
    let mut seen = result.to_string();
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let file = file.unwrap().path();
        if file.is_file() {
            seen.push_str(&String::from_utf8_lossy(&std::fs::read(file).unwrap()));
        }
    }
    for secret in &SECRETS[1..] {
        assert!(
            !seen.contains(secret),
            "{secret} reached saved state or MCP output"
        );
    }
}

#[tokio::test]
async fn binding_refuses_a_directory_owned_by_another_account() {
    let server = MockServer::start().await;
    apple(&server).await;
    let dir = tempfile::tempdir().unwrap();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state(
            quince_sync::queue::ACCOUNT_KEY,
            &quince_sync::queue::account_tag("9999999999"),
        )
        .unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut answers = [SECRETS[0], SECRETS[1], "1", SECRETS[2]].into_iter();
    skykeep_mcp::login::authenticate(&mut client, &mut |_| {
        Ok(zeroize::Zeroizing::new(answers.next().unwrap().to_string()))
    })
    .await
    .unwrap();
    assert_eq!(
        skykeep_mcp::login::bind_account(dir.path(), &mut client),
        Err("account_mismatch_use_new_state_directory")
    );
    // The mismatched login is signed out, so no session for the wrong account is left to resume.
    let left = Store::file(dir.path()).unwrap().try_load().unwrap();
    assert!(!left.is_resumable());
    assert_eq!(
        quince_core::Library::open(dir.path())
            .unwrap()
            .state(quince_sync::queue::ACCOUNT_KEY)
            .unwrap(),
        Some(quince_sync::queue::account_tag("9999999999"))
    );
}

#[tokio::test]
async fn binding_without_a_completed_login_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at("http://127.0.0.1:1"),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    assert_eq!(
        skykeep_mcp::login::bind_account(dir.path(), &mut client),
        Err("login_incomplete")
    );
    assert_eq!(
        quince_core::Library::open(dir.path())
            .unwrap()
            .state(quince_sync::queue::ACCOUNT_KEY)
            .unwrap(),
        None
    );
}
