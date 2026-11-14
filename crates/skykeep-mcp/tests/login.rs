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
        "0",
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
        "0",
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
        "7",
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
    labels
}

fn sms_only_options() -> serde_json::Value {
    json!({"direct": {"hasTrustedDevices": false, "twoSV": {
        "bridgeInitiateData": {"phoneNumberVerification": {
            "trustedPhoneNumbers": [{"id": 7, "pushMode": "sms"}]
        }}
    }}})
}

#[tokio::test]
async fn unavailable_and_malformed_routes_retry_sms_without_new_credentials() {
    let labels = route_case(
        sms_only_options(),
        &[
            Ok("0"),
            Ok(""),
            Ok("   "),
            Ok("not-a-route"),
            Ok("18446744073709551616"),
            Ok("99"),
            Ok("7"),
            Ok("123456"),
        ],
        Ok(()),
        Some(7),
    )
    .await;
    for label in &labels[2..labels.len() - 1] {
        assert!(!label.contains("0 for trusted device"), "{label}");
        assert!(label.contains("[7]"), "{label}");
    }
    assert!(labels[2].contains("Trusted device unavailable"));
    assert!(labels[3].contains("Invalid or unavailable code route"));
}

#[tokio::test]
async fn malformed_route_can_retry_an_existing_sms_route() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1,
        "trustedPhoneNumbers": [{"id": 7}]}),
        &[Ok("bad"), Ok("7"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(labels[2].contains("0 for trusted device"));
}

#[tokio::test]
async fn no_eligible_code_routes_fail_before_route_prompt() {
    route_case(
        json!({"trustedDeviceCount": 0, "trustedPhoneNumbers": []}),
        &[],
        Err("no_available_code_routes"),
        None,
    )
    .await;
}

#[tokio::test]
async fn route_retry_preserves_cancellation_and_timeout() {
    for error in ["input_cancelled", "input_timeout"] {
        route_case(sms_only_options(), &[Ok("0"), Err(error)], Err(error), None).await;
    }
}

#[tokio::test]
async fn sms_zero_is_selectable_with_and_without_trusted_devices() {
    for devices in [0, 1] {
        let labels = route_case(
            json!({"trustedDeviceCount": devices, "trustedPhoneNumbers": [{"id": 0}]}),
            &[Ok("bad"), Ok("sms:0"), Ok("123456")],
            Ok(()),
            Some(0),
        )
        .await;
        assert!(labels[2].contains("sms:0"), "{}", labels[2]);
        assert_eq!(labels[2].contains("0 for trusted device"), devices > 0);
    }
}

#[tokio::test]
async fn trusted_zero_remains_distinct_from_sms_zero() {
    let labels = route_case(
        json!({"trustedDeviceCount": 1, "trustedPhoneNumbers": [{"id": 0}]}),
        &[Ok("0"), Ok("123456")],
        Ok(()),
        None,
    )
    .await;
    assert!(labels[2].contains("0 for trusted device"));
    assert!(labels[2].contains("sms:0"));
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
async fn unadvertised_sms_zero_retries_an_offered_positive_phone_id() {
    let labels = route_case(
        sms_only_options(),
        &[Ok("sms:0"), Ok("7"), Ok("123456")],
        Ok(()),
        Some(7),
    )
    .await;
    assert!(!labels[2].contains("sms:0"));
    assert!(labels[3].contains("Invalid or unavailable code route"));
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
