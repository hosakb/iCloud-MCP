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
