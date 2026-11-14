use quince_auth::{session::Session, Store};

#[test]
fn locked_scoped_keyring_never_launches_an_unlock_prompt() {
    assert!(!quince_auth::session::may_unlock("skykeep-mcp"));
    assert!(quince_auth::session::may_unlock("desktop-example"));
}

#[tokio::test]
async fn pending_passwordless_auth_does_not_persist_challenge_tokens() {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/appleauth/auth/signin/init"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"salt":B64.encode([0x77;16]),"b":B64.encode([0x5e;256]),"c":"challenge","iteration":10,"protocol":"s2k"}))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/appleauth/auth/signin/complete"))
        .respond_with(
            ResponseTemplate::new(409)
                .insert_header("X-Apple-Session-Token", "SYNTHETIC-PENDING-TOKEN")
                .set_body_json(json!({})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = quince_auth::Client::with(
        dir.path(),
        quince_auth::Endpoints::at(&server.uri()),
        Store::file(dir.path()).unwrap().without_passwords(),
    )
    .unwrap();
    assert_eq!(
        client
            .sign_in("fixture@example.invalid", "synthetic-password")
            .await
            .unwrap(),
        quince_auth::SignInOutcome::NeedsTwoFactor
    );
    assert!(
        !dir.path().join("session.json").exists(),
        "pending auth must not create a resumable session"
    );
    assert!(!dir.path().join("cookies.json").exists());
}

#[test]
fn isolated_store_never_retains_passwords() {
    let dir = tempfile::tempdir().unwrap();
    // File backend is a synthetic fixture, never the production secret store.
    let store = Store::file(dir.path()).unwrap().without_passwords();
    store
        .save(&Session {
            apple_id: Some("fixture@example.invalid".into()),
            password: Some("DO-NOT-PERSIST-SYNTHETIC-PASSWORD".into()),
            trust_token: Some("synthetic-trust".into()),
            ..Default::default()
        })
        .unwrap();
    let bytes = std::fs::read_to_string(dir.path().join("session.json")).unwrap();
    assert!(!bytes.contains("DO-NOT-PERSIST-SYNTHETIC-PASSWORD"));
    let loaded = store.try_load().unwrap();
    assert!(loaded.password.is_none());
    assert_eq!(loaded.trust_token.as_deref(), Some("synthetic-trust"));
}

#[test]
fn passwordless_store_discards_preexisting_password_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::file(dir.path()).unwrap();
    store
        .save(&Session {
            password: Some("synthetic-legacy".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(store
        .without_passwords()
        .try_load()
        .unwrap()
        .password
        .is_none());
}
