//! The OS keyring as the desktop's [`CredentialStore`].
//!
//! Every provider shares this store (Google Drive and SoundCloud alike); each
//! secret is one keyring entry under service `rec_bag`, user
//! `{provider}:{account}`.
//!
//! `soundcloud_catalog.rs` has its own, unused `KeyringCredentialStore` that
//! addresses entries differently (service `rec_bag:{provider}`, user
//! `{account}`). They are deliberately not merged: switching either format
//! would orphan the keyring entries already written in the other.

use super::integration::{CredentialFuture, CredentialKey, CredentialStore, Secret};
use crate::state::integration::IntegrationError;

pub struct KeyringCredentialStore {
    service: String,
}

impl KeyringCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &CredentialKey) -> Result<keyring::Entry, IntegrationError> {
        keyring::Entry::new(&self.service, &format!("{}:{}", key.provider, key.account)).map_err(
            |error| IntegrationError {
                request_id: None,
                slice: None,
                message: format!("credential store is unavailable: {error}"),
                retryable: true,
            },
        )
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn load<'a>(&'a self, key: &'a CredentialKey) -> CredentialFuture<'a, Option<Secret>> {
        Box::pin(async move {
            let entry = self.entry(key)?;
            match entry.get_secret() {
                Ok(secret) => Ok(Some(Secret::new(secret))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(IntegrationError {
                    request_id: None,
                    slice: None,
                    message: format!("credential read failed: {error}"),
                    retryable: true,
                }),
            }
        })
    }

    fn store<'a>(&'a self, key: &'a CredentialKey, secret: Secret) -> CredentialFuture<'a, ()> {
        Box::pin(async move {
            self.entry(key)?
                .set_secret(secret.expose())
                .map_err(|error| IntegrationError {
                    request_id: None,
                    slice: None,
                    message: format!("credential write failed: {error}"),
                    retryable: true,
                })
        })
    }

    fn delete<'a>(&'a self, key: &'a CredentialKey) -> CredentialFuture<'a, ()> {
        Box::pin(async move {
            match self.entry(key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(IntegrationError {
                    request_id: None,
                    slice: None,
                    message: format!("credential deletion failed: {error}"),
                    retryable: true,
                }),
            }
        })
    }
}
