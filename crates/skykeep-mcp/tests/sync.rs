use quince_auth::{session::Session, Client, Endpoints, Store};
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn authenticated_sync_refreshes_only_notes_and_reminders() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{"ckdatabasews":{"url":server.uri()}}}))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path_regex(".*/changes/zone$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"zones":[{"records":[],"syncToken":"synthetic-cursor","moreComing":false}]}),
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::file(dir.path()).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some("1234567890".into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":server.uri()}})),
            ..Default::default()
        })
        .unwrap();
    let client = Client::with(dir.path(), Endpoints::at(&server.uri()), store).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    let result = skykeep_mcp::sync::with_client(dir.path(), client)
        .await
        .unwrap();
    assert_eq!(result["services"].as_array().unwrap().len(), 2);
    assert_eq!(result["remote_verified"], false);
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .any(|r| r.url.path().contains("com.apple.notes")));
    assert!(requests
        .iter()
        .any(|r| r.url.path().contains("com.apple.reminders")));
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() == "/setup/ws/1/validate"
                || r.url.path().contains("com.apple.notes")
                || r.url.path().contains("com.apple.reminders")),
        "unexpected service access"
    );
    let result = result.to_string();
    assert!(!result.contains("SYNTHETIC-TOKEN"));
}

fn saved_client(dir: &std::path::Path, uri: &str) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some("1234567890".into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":uri}})),
            ..Default::default()
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

#[tokio::test]
async fn expired_session_returns_login_required_without_service_access() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(
            ResponseTemplate::new(421).set_body_json(json!({"error":"SYNTHETIC-EXPIRED-BODY"})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let client = saved_client(dir.path(), &server.uri());
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("1234567890"))
        .unwrap();
    let error = skykeep_mcp::sync::with_client(dir.path(), client)
        .await
        .unwrap_err();
    assert!(error.starts_with("login_required"), "{error}");
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.iter().all(|r| !r.url.path().contains("com.apple")),
        "no data access after expiry"
    );
}

#[tokio::test]
async fn session_for_another_account_never_reads_or_sends() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"dsInfo":{"dsid":"1234567890"},"webservices":{"ckdatabasews":{"url":server.uri()}}}))).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let client = saved_client(dir.path(), &server.uri());
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    assert_eq!(
        skykeep_mcp::sync::with_client(dir.path(), client)
            .await
            .unwrap_err(),
        "account_mismatch_use_new_state_directory"
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().contains("com.apple")));
}
