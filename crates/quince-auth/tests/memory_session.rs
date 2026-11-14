use quince_auth::{session::Session, Client, Endpoints, Store};

#[test]
fn memory_store_never_resumes_or_persists_secrets() {
    let store = Store::memory();
    let session = Session {
        password: Some("synthetic-password".into()),
        session_token: Some("synthetic-token".into()),
        ..Default::default()
    };
    store.save(&session).unwrap();
    assert!(!store.try_load().unwrap().is_resumable());
    store.clear().unwrap();
    assert!(!store.sign_out_pending());
}

#[test]
fn memory_client_does_not_read_existing_session_or_cookies() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["session.json", "cookies.json", "sign-out-pending"] {
        std::fs::write(dir.path().join(name), b"leave this file alone").unwrap();
    }
    let client = Client::with(dir.path(), Endpoints::default(), Store::memory()).unwrap();
    assert!(!client.has_stored_session());
    for name in ["session.json", "cookies.json", "sign-out-pending"] {
        assert_eq!(
            std::fs::read(dir.path().join(name)).unwrap(),
            b"leave this file alone"
        );
    }
}
