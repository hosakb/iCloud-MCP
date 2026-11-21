//! Secrets the caller keeps ([`super::Store::with_secrets`]), read and written as the keyring
//! backend reads and writes them.
use super::{read_secret, write_secret, PlatformSecrets, Secrets};

/// The service name every given store is asked under.
const SERVICE: &str = "skykeep-mcp";

pub(super) struct Given<'a>(pub(super) &'a dyn PlatformSecrets);

impl Secrets for Given<'_> {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        self.0.get(SERVICE, name)
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.0.set(SERVICE, name, value)
    }
    fn delete(&self, name: &str) -> Result<(), String> {
        self.0.delete(SERVICE, name)
    }
}

/// Saved whole, then read back: a store that said yes and kept nothing fails here, not at the
/// next start.
pub(super) fn save(
    given: &dyn PlatformSecrets,
    name: &str,
    json: &str,
) -> Result<(), crate::Error> {
    let k = Given(given);
    write_secret(&k, name, json, None).map_err(crate::Error::Keyring)?;
    match read_secret(&k, name) {
        Ok(Some(back)) if back == json => Ok(()),
        Ok(_) => Err(crate::Error::Keyring(
            "the secret store accepted the sign-in but did not keep it".into(),
        )),
        Err(e) => Err(crate::Error::Keyring(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Session, Store};
    use super::PlatformSecrets;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<HashMap<String, String>>>);
    impl PlatformSecrets for Fake {
        fn get(&self, service: &str, name: &str) -> Result<Option<String>, String> {
            assert_eq!(service, "skykeep-mcp");
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&self, _: &str, name: &str, value: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, _: &str, name: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    #[test]
    fn a_given_store_keeps_the_session_without_its_password_and_forgets_it_on_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let store = Store::with_secrets(dir.path(), Box::new(fake.clone())).unwrap();
        assert!(store.is_passwordless());
        let session = Session {
            session_token: Some("SYNTHETIC-TOKEN".into()),
            password: Some("SYNTHETIC-PASSWORD".into()),
            ..Session::default()
        };
        store.save(&session).unwrap();
        assert!(
            !dir.path().join("session.json").exists(),
            "nothing in a plain file"
        );
        let back = store.try_load().unwrap();
        assert_eq!(back.session_token.as_deref(), Some("SYNTHETIC-TOKEN"));
        assert!(back.password.is_none());
        assert!(fake
            .0
            .lock()
            .unwrap()
            .values()
            .all(|v| !v.contains("SYNTHETIC-PASSWORD")));
        store.clear().unwrap();
        assert!(fake.0.lock().unwrap().is_empty());
        assert!(!store.try_load().unwrap().is_resumable());
    }
}
