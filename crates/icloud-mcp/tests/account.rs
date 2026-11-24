use icloud_mcp::account::{binding, Binding};
use quince_sync::queue::{account_tag, ACCOUNT_KEY};

#[test]
fn binding_compares_the_stored_tag_with_the_signed_in_account() {
    let dir = tempfile::tempdir().unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    assert_eq!(
        binding(&library, "111").unwrap(),
        Binding::Unbound(account_tag("111"))
    );
    library.set_state(ACCOUNT_KEY, &account_tag("111")).unwrap();
    assert_eq!(binding(&library, "111").unwrap(), Binding::Same);
    assert_eq!(binding(&library, "222").unwrap(), Binding::Other);
}
