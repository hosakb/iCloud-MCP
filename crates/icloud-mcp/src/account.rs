//! A private directory belongs to one Apple Account, identified by a one-way tag.
use quince_core::Library;
use quince_sync::queue::{account_tag, ACCOUNT_KEY};

#[derive(Debug, PartialEq, Eq)]
pub enum Binding {
    /// No account yet; holds the tag to store for this account.
    Unbound(String),
    Same,
    Other,
}

/// Compares the directory's stored account tag with the signed-in account's DSID.
pub fn binding(library: &Library, dsid: &str) -> Result<Binding, &'static str> {
    let tag = account_tag(dsid);
    Ok(
        match library
            .state(ACCOUNT_KEY)
            .map_err(|_| "library_unavailable")?
        {
            None => Binding::Unbound(tag),
            Some(stored) if stored == tag => Binding::Same,
            Some(_) => Binding::Other,
        },
    )
}
