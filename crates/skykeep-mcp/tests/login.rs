use base64::{engine::general_purpose::STANDARD as B64, Engine};
use quince_auth::{Client, Endpoints, Store};
use serde_json::json;
use skykeep_mcp::login::authenticate;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

async fn mock_auth(server: &MockServer) {
    for (verb, endpoint, response) in [
        ("POST", "/appleauth/auth/signin/init", ResponseTemplate::new(200).set_body_json(json!({"salt":B64.encode([0x77;16]),"b":B64.encode([0x5e;256]),"c":"challenge","iteration":10,"protocol":"s2k"}))),
        ("POST", "/appleauth/auth/signin/complete", ResponseTemplate::new(409).insert_header("X-Apple-Session-Token","SYNTHETIC-PENDING-TOKEN").set_body_json(json!({}))),
        ("PUT", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth", ResponseTemplate::new(200).set_body_json(json!({"trustedDeviceCount":1,"trustedPhoneNumbers":[{"id":7,"numberWithDialCode":"+00 ***123","pushMode":"sms"}]}))),
        ("POST", "/appleauth/auth/verify/trusteddevice/securitycode", ResponseTemplate::new(204)),
        ("GET", "/appleauth/auth/2sv/trust", ResponseTemplate::new(204).insert_header("X-Apple-TwoSV-Trust-Token","SYNTHETIC-TRUST").insert_header("X-Apple-Session-Token","SYNTHETIC-COMPLETE-TOKEN")),
        ("POST", "/setup/ws/1/accountLogin", ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{}}))),
    ] { Mock::given(method(verb)).and(path(endpoint)).respond_with(response).mount(server).await; }
}

#[tokio::test]
async fn trusted_device_login_persists_only_completed_passwordless_session() {
    let server = MockServer::start().await;
    mock_auth(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut inputs = [
        "fixture@example.invalid",
        "SYNTHETIC-PASSWORD",
        "1",
        "123456",
    ]
    .into_iter();
    let result = authenticate(&mut client, &mut |_| {
        Ok(Zeroizing::new(inputs.next().unwrap().to_string()))
    })
    .await;
    assert!(
        result.is_ok(),
        "{result:?}; paths {:?}",
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path())
            .collect::<Vec<_>>()
    );
    assert_eq!(client.session().dsid.as_deref(), Some("1234567890"));
    assert!(
        client.session().password.is_none(),
        "passwordless consumers must not retain renewal passwords in memory after login"
    );
    let stored = Store::file(dir.path()).unwrap().try_load().unwrap();
    assert!(stored.password.is_none());
    assert_eq!(stored.trust_token.as_deref(), Some("SYNTHETIC-TRUST"));
}

#[tokio::test]
async fn wrong_code_retries_without_restarting_or_retaining_password() {
    let server = MockServer::start().await;
    mock_auth(&server).await;
    Mock::given(method("POST"))
        .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"serviceErrors":[{"code":"-21669","message":"SYNTHETIC-ERROR-SENTINEL"}]}),
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut inputs = [
        "fixture@example.invalid",
        "SYNTHETIC-PASSWORD",
        "1",
        "111111",
        "123456",
    ]
    .into_iter();
    let result = authenticate(&mut client, &mut |_| {
        Ok(Zeroizing::new(inputs.next().unwrap().to_string()))
    })
    .await;
    assert_eq!(result, Ok(()));
    assert!(inputs.next().is_none());
}

#[tokio::test]
async fn sms_route_uses_only_an_offered_phone_id() {
    let server = MockServer::start().await;
    mock_auth(&server).await;
    for (verb, endpoint) in [
        ("PUT", "/appleauth/auth/verify/phone"),
        ("POST", "/appleauth/auth/verify/phone/securitycode"),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut inputs = [
        "fixture@example.invalid",
        "SYNTHETIC-PASSWORD",
        "2",
        "123456",
    ]
    .into_iter();
    let result = authenticate(&mut client, &mut |_| {
        Ok(Zeroizing::new(inputs.next().unwrap().to_string()))
    })
    .await;
    assert_eq!(result, Ok(()));
    let requests = server.received_requests().await.unwrap();
    let sent: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().contains("/verify/phone"))
        .collect();
    assert_eq!(sent.len(), 2);
    for r in sent {
        assert_eq!(
            r.body_json::<serde_json::Value>().unwrap()["phoneNumber"]["id"],
            7
        );
    }
}

// All options and inputs below are synthetic, never a captured account response.
async fn route_case(
    options: serde_json::Value,
    answers: &[Result<&str, &'static str>],
    expected: Result<(), &'static str>,
    expected_sms: Option<u64>,
) -> Vec<String> {
    let server = MockServer::start().await;
    mock_auth(&server).await;
    Mock::given(method("GET"))
        .and(path("/appleauth/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(options))
        .with_priority(1)
        .mount(&server)
        .await;
    for (verb, endpoint) in [
        ("PUT", "/appleauth/auth/verify/phone"),
        ("POST", "/appleauth/auth/verify/phone/securitycode"),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut inputs = [Ok("fixture@example.invalid"), Ok("SYNTHETIC-PASSWORD")]
        .into_iter()
        .chain(answers.iter().copied());
    let mut labels = Vec::new();
    let result = authenticate(&mut client, &mut |label| {
        labels.push(label.to_string());
        inputs
            .next()
            .expect("unexpected extra prompt")
            .map(|s| Zeroizing::new(s.to_string()))
    })
    .await;
    assert_eq!(result, expected);
    assert!(inputs.next().is_none(), "all route inputs consumed");
    assert_eq!(
        labels
            .iter()
            .filter(|l| l.starts_with("Apple Account ("))
            .count(),
        1
    );
    assert_eq!(
        labels
            .iter()
            .filter(|l| l.starts_with("Apple Account password"))
            .count(),
        1
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/signin/init"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/signin/complete"))
            .count(),
        1
    );
    let phone_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().contains("/verify/phone"))
        .collect();
    if expected.is_ok() {
        assert_eq!(
            phone_requests.len(),
            if expected_sms.is_some() { 2 } else { 0 }
        );
        for request in phone_requests {
            let body = request.body_json::<serde_json::Value>().unwrap();
            assert_eq!(body["phoneNumber"]["id"], expected_sms.unwrap());
            if request.url.path().ends_with("/securitycode") {
                assert_eq!(request.method, "POST");
                assert_eq!(body["securityCode"]["code"], "123456");
            } else {
                assert_eq!(request.method, "PUT");
                assert_eq!(body["mode"], "sms");
            }
        }
        assert_eq!(client.session().dsid.as_deref(), Some("1234567890"));
        assert!(client.session().password.is_none());
    } else {
        assert!(phone_requests.is_empty());
        assert!(!dir.path().join("session.json").exists());
    }
    // Only exclude trusted-device POST verification for SMS/error cases. sign_in already
    // attempts a trusted-device PUT dispatch before fetching the available routes.
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.url.path().contains("/verify/trusteddevice"))
            .count(),
        usize::from(expected.is_ok() && expected_sms.is_none())
    );
    if expected.is_ok() && expected_sms.is_none() {
        // Apple waits to be asked (PUT) before pushing a code to trusted devices; the
        // verification POST must follow that request.
        let put = requests.iter().position(|r| {
            r.method == "PUT" && r.url.path().ends_with("/verify/trusteddevice/securitycode")
        });
        let post = requests.iter().position(|r| {
            r.method == "POST" && r.url.path().ends_with("/verify/trusteddevice/securitycode")
        });
        assert!(
            put.is_some() && put < post,
            "PUT {put:?} before POST {post:?}"
        );
        let body = requests[post.unwrap()]
            .body_json::<serde_json::Value>()
            .unwrap();
        assert_eq!(body["securityCode"]["code"], "123456");
    }
    labels
}

fn sms_only_options() -> serde_json::Value {
    json!({"direct": {"hasTrustedDevices": false, "twoSV": {
        "bridgeInitiateData": {"phoneNumberVerification": {
            "trustedPhoneNumbers": [{"id": 7, "pushMode": "sms"}]
        }}
    }}})
}

const ZERO_DEVICE_NOTE: &str = "Apple reported no trusted devices";
const DEVICES: &str = "1. Send the code to my Apple devices";
const INVALID: &str = "That is not one of the choices";

#[tokio::test]
async fn malformed_routes_retry_sms_without_new_credentials() {
    let labels = route_case(
        sms_only_options(),
        &[
            Ok(""),
            Ok("   "),
            Ok("not-a-route"),
            Ok("18446744073709551616"),
            Ok("99"),
            Ok("0"),
            Ok("2"),
            Ok("123456"),
        ],
        Ok(()),
        Some(7),
    )
    .await;
    for label in &labels[2..labels.len() - 1] {
        assert!(label.contains(DEVICES), "{label}");
        // Apple gave no number for this phone, so none is made up.
        assert!(
            label.contains("2. Send a text message to trusted phone number 1"),
            "{label}"
        );
    }
    assert!(labels[2].contains(ZERO_DEVICE_NOTE), "{}", labels[2]);
    assert!(labels[3].contains(INVALID));
}

// Apple's count is not a reliable gate: a live account reported zero trusted devices while a
// code still arrived on its trusted device. The original app always offers that route.
#[tokio::test]
async fn trusted_device_route_works_when_apple_reports_zero_devices() {
    let labels = route_case(sms_only_options(), &[Ok("1"), Ok("123456")], Ok(()), None).await;
    assert!(labels[2].contains(DEVICES), "{}", labels[2]);
    assert!(labels[2].contains(ZERO_DEVICE_NOTE), "{}", labels[2]);
}

#[tokio::test]
async fn trusted_device_route_works_with_zero_devices_and_no_phones() {
    let labels = route_case(
        json!({"trustedDeviceCount": 0, "trustedPhoneNumbers": []}),
        &[Ok("1"), Ok("123456")],
        Ok(()),
        None,
    )
    .await;
    assert!(labels[2].contains(DEVICES), "{}", labels[2]);
    assert!(!labels[2].contains("2."), "{}", labels[2]);
    assert!(labels[2].contains(ZERO_DEVICE_NOTE), "{}", labels[2]);
}

#[tokio::test]
async fn reported_trusted_devices_omit_zero_device_note() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1, "trustedPhoneNumbers": []}),
        &[Ok("1"), Ok("123456")],
        Ok(()),
        None,
    )
    .await;
    assert!(!labels[2].contains(ZERO_DEVICE_NOTE), "{}", labels[2]);
}

#[tokio::test]
async fn malformed_route_can_retry_an_existing_sms_route() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1,
        "trustedPhoneNumbers": [{"id": 7}]}),
        &[Ok("bad"), Ok("2"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(labels[2].contains(DEVICES));
}

// A security-key-only answer offers no code at all; the original app shows its security-key
// screen instead of a code prompt. Keep refusing before any route prompt.
#[tokio::test]
async fn security_key_only_answer_fails_before_route_prompt() {
    route_case(
        json!({"fsaChallenge": {"challenge": "Y2hhbGxlbmdl", "keyHandles": ["aGFuZGxl"], "rpId": "apple.com"}}),
        &[],
        Err("security_key_sign_in_unsupported"),
        None,
    )
    .await;
}

#[tokio::test]
async fn route_retry_preserves_cancellation_and_timeout() {
    for error in ["input_cancelled", "input_timeout"] {
        route_case(
            sms_only_options(),
            &[Ok("99"), Err(error)],
            Err(error),
            None,
        )
        .await;
    }
}

#[tokio::test]
async fn phone_id_zero_is_selectable_with_and_without_trusted_devices() {
    for devices in [0, 1] {
        let labels = route_case(
            json!({"trustedDeviceCount": devices, "trustedPhoneNumbers": [{"id": 0}]}),
            &[Ok("bad"), Ok("2"), Ok("123456")],
            Ok(()),
            Some(0),
        )
        .await;
        assert!(
            labels[2].contains("2. Send a text message"),
            "{}",
            labels[2]
        );
        assert!(labels[2].contains(DEVICES), "{}", labels[2]);
    }
}

#[tokio::test]
async fn trusted_devices_remain_distinct_from_phone_id_zero() {
    for devices in [0, 1] {
        let labels = route_case(
            json!({"trustedDeviceCount": devices, "trustedPhoneNumbers": [{"id": 0}]}),
            &[Ok("1"), Ok("123456")],
            Ok(()),
            None,
        )
        .await;
        assert!(labels[2].contains(DEVICES), "{}", labels[2]);
        assert!(
            labels[2].contains("2. Send a text message"),
            "{}",
            labels[2]
        );
    }
}

#[tokio::test]
async fn zero_phone_route_retries_preserve_cancellation_and_timeout() {
    for devices in [0, 1] {
        for error in ["input_cancelled", "input_timeout"] {
            route_case(
                json!({"trustedDeviceCount": devices, "trustedPhoneNumbers": [{"id": 0}]}),
                &[Ok("99"), Err(error)],
                Err(error),
                None,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn a_choice_that_is_not_listed_retries_an_offered_one() {
    let labels = route_case(
        sms_only_options(),
        &[Ok("3"), Ok("2"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(!labels[2].contains("3."));
    assert!(labels[3].contains(INVALID));
}

#[tokio::test]
async fn apple_masked_numbers_are_shown_as_apple_sent_them_and_calls_say_so() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1, "trustedPhoneNumbers": [
            {"id": 7, "numberWithDialCode": "+00 \u{2022}\u{2022}\u{2022}23", "pushMode": "sms"},
            {"id": 8, "numberWithDialCode": "+00 \u{2022}\u{2022}\u{2022}45\u{202e}\u{1b}[2J", "pushMode": "voice"}
        ]}),
        &[Ok("2"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(
        labels[2].contains("2. Send a text message to +00 \u{2022}\u{2022}\u{2022}23"),
        "{}",
        labels[2]
    );
    assert!(
        labels[2].contains("3. Call me at +00 \u{2022}\u{2022}\u{2022}45"),
        "{}",
        labels[2]
    );
    assert!(!labels[2].contains('\u{202e}') && !labels[2].contains('\u{1b}'));
    // Apple's answer names no devices, so none are named.
    assert!(labels[2].contains(DEVICES));
}

#[tokio::test]
async fn going_back_from_the_code_switches_route_without_new_credentials() {
    let labels = route_case(
        sms_only_options(),
        &[Ok("1"), Ok("b"), Ok("2"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(
        labels[3].contains("type b to choose another way"),
        "{}",
        labels[3]
    );
    assert!(
        labels[4].starts_with("Where should Apple send"),
        "{}",
        labels[4]
    );
}

#[tokio::test]
async fn code_sends_are_bounded_and_the_last_code_prompt_offers_no_way_back() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1, "trustedPhoneNumbers": []}),
        &[
            Ok("1"),
            Ok("b"),
            Ok("1"),
            Ok("b"),
            Ok("1"),
            Ok("b"),
            Ok("1"),
            Ok("123456"),
        ],
        Ok(()),
        None,
    )
    .await;
    let last = &labels[labels.len() - 1];
    assert!(last.starts_with("Apple verification code"), "{last}");
    assert!(!last.contains("type b"), "{last}");
}

#[tokio::test]
async fn cancelled_login_cannot_save_partial_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::with(
        dir.path(),
        Endpoints::at("http://127.0.0.1:1"),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut prompt =
        |_: &str| -> Result<Zeroizing<String>, &'static str> { Err("input_cancelled") };
    assert_eq!(
        authenticate(&mut client, &mut prompt).await,
        Err("input_cancelled")
    );
    assert!(!dir.path().join("session.json").exists());
}

#[tokio::test]
async fn different_account_is_rejected_before_network() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::file(dir.path()).unwrap().without_passwords();
    store
        .save(&quince_auth::session::Session {
            apple_id: Some("original@example.invalid".into()),
            ..Default::default()
        })
        .unwrap();
    let mut client = Client::with(dir.path(), Endpoints::at("http://127.0.0.1:1"), store).unwrap();
    let mut prompt = |_: &str| Ok(Zeroizing::new("different@example.invalid".to_string()));
    assert_eq!(
        authenticate(&mut client, &mut prompt).await,
        Err("account_mismatch_use_new_state_directory")
    );
}

/// Choosing Apple devices again after going back asks Apple for a fresh device code.
#[tokio::test]
async fn choosing_devices_again_requests_a_fresh_device_code() {
    async fn device_pushes(answers: &[&str]) -> usize {
        let server = MockServer::start().await;
        mock_auth(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let mut client = Client::with(
            dir.path(),
            Endpoints::at(&server.uri()),
            Store::file(dir.path()).unwrap().without_passwords(),
        )
        .unwrap();
        let mut inputs = ["fixture@example.invalid", "SYNTHETIC-PASSWORD"]
            .iter()
            .chain(answers.iter());
        let result = authenticate(&mut client, &mut |_| {
            Ok(Zeroizing::new(inputs.next().unwrap().to_string()))
        })
        .await;
        assert_eq!(result, Ok(()));
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.method == "PUT" && r.url.path().ends_with("/verify/trusteddevice/securitycode")
            })
            .count()
    }
    let once = device_pushes(&["1", "123456"]).await;
    let twice = device_pushes(&["1", "b", "1", "123456"]).await;
    assert_eq!(twice, once + 1, "a second push for the second choice");
}

/// `skykeep-mcp login` binds the account the same way setup does, and a new sign-in clears
/// an earlier failed check, so auth_status no longer says "not signed in".
#[tokio::test]
async fn a_new_sign_in_clears_an_earlier_failed_check() {
    use std::os::unix::fs::PermissionsExt;
    // As the program itself runs: new files private to this user.
    unsafe {
        libc::umask(0o077);
    }
    let server = MockServer::start().await;
    mock_auth(&server).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut mcp = skykeep_mcp::Server::open(dir.path()).unwrap();
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    skykeep_mcp::note_session_check(
        &skykeep_mcp::state::PrivateState::open(dir.path()).unwrap(),
        Err("login_required"),
    );
    let auth = |reply: serde_json::Value| -> serde_json::Value {
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    mcp.handle(json!({"id":0,"method":"initialize"})).await;
    let request =
        json!({"id":1,"method":"tools/call","params":{"name":"auth_status","arguments":{}}});
    let before = auth(mcp.handle(request.clone()).await.unwrap());
    assert_eq!(before["status"], "login_required", "{before}");

    let mut client = Client::with(
        dir.path(),
        Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    let mut inputs = [
        "fixture@example.invalid",
        "SYNTHETIC-PASSWORD",
        "1",
        "123456",
    ]
    .into_iter();
    authenticate(&mut client, &mut |_| {
        Ok(Zeroizing::new(inputs.next().unwrap().to_string()))
    })
    .await
    .unwrap();
    skykeep_mcp::login::bind_account(dir.path(), &mut client).unwrap();
    let after = auth(mcp.handle(request).await.unwrap());
    assert_eq!(after["status"], "session_unverified", "{after}");
}
