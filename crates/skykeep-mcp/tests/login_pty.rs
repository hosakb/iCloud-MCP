//! Drives the real hidden prompt and login state machine over a test-owned PTY against loopback
//! fixtures. Every value is fictional.
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use quince_auth::{Client, Endpoints, Store};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[tokio::test(flavor = "multi_thread")]
async fn terminal_login_transcript_never_shows_synthetic_secrets() {
    let server = MockServer::start().await;
    for (verb, endpoint, response) in [
        ("POST", "/appleauth/auth/signin/init", ResponseTemplate::new(200).set_body_json(json!({"salt":B64.encode([0x77;16]),"b":B64.encode([0x5e;256]),"c":"challenge","iteration":10,"protocol":"s2k"}))),
        ("POST", "/appleauth/auth/signin/complete", ResponseTemplate::new(409).insert_header("X-Apple-Session-Token","SYNTHETIC-PENDING-TOKEN").set_body_json(json!({}))),
        ("PUT", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth", ResponseTemplate::new(200).set_body_json(json!({"trustedDeviceCount":1,"trustedPhoneNumbers":[]}))),
        ("POST", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth/2sv/trust", ResponseTemplate::new(204).insert_header("X-Apple-TwoSV-Trust-Token","SYNTHETIC-TRUST").insert_header("X-Apple-Session-Token","SYNTHETIC-COMPLETE-TOKEN")),
        ("POST", "/setup/ws/1/accountLogin", ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{}}))),
    ] { Mock::given(method(verb)).and(path(endpoint)).respond_with(response).mount(&server).await; }

    let (mut master, mut slave) = pair();
    let typist = std::thread::spawn(move || {
        let mut transcript = Vec::new();
        for answer in [
            "fixture@example.invalid",
            "SYNTHETIC-PASSWORD-SENTINEL",
            "1",
            "424242",
        ] {
            // Answer only once a whole prompt is shown; TCSAFLUSH discards early typing.
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
        transcript
    });
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let result = skykeep_mcp::login::authenticate(&mut client, &mut |label| {
        skykeep_mcp::terminal::hidden(&mut slave, label, Duration::from_secs(5))
    })
    .await;
    assert_eq!(result, Ok(()));
    let transcript = String::from_utf8_lossy(&typist.join().unwrap()).to_string();
    std::fs::write(dir.path().join("transcript.txt"), &transcript).unwrap();
    if let Ok(out) = std::env::var("SKYKEEP_TRANSCRIPT_OUT") {
        std::fs::write(out, &transcript).unwrap();
    }
    assert!(
        transcript.contains("Apple verification code"),
        "{transcript}"
    );
    for secret in [
        "fixture@example.invalid",
        "SYNTHETIC-PASSWORD-SENTINEL",
        "424242",
    ] {
        assert!(!transcript.contains(secret), "terminal echoed {secret}");
    }
    let saved = std::fs::read_to_string(dir.path().join("session.json")).unwrap();
    for secret in ["SYNTHETIC-PASSWORD-SENTINEL", "424242"] {
        assert!(!saved.contains(secret));
    }
}
