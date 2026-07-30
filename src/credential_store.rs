use anyhow::{Context, Result};

const SERVICE: &str = "tech.disc.cli.oauth";

pub trait CredentialStore: Send + Sync {
    fn get_refresh_token(&self, account: &str) -> Result<String>;
    fn set_refresh_token(&self, account: &str, refresh_token: &str) -> Result<()>;
    fn delete_refresh_token(&self, account: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCredentialStore;

impl SystemCredentialStore {
    fn entry(account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, account)
            .context("Failed to open the operating-system credential store.")
    }
}

impl CredentialStore for SystemCredentialStore {
    fn get_refresh_token(&self, account: &str) -> Result<String> {
        Self::entry(account)?
            .get_password()
            .context("Failed to read the Disc OAuth refresh token from the operating-system credential store.")
    }

    fn set_refresh_token(&self, account: &str, refresh_token: &str) -> Result<()> {
        Self::entry(account)?.set_password(refresh_token).context(
            "Failed to save the Disc OAuth refresh token in the operating-system credential store.",
        )
    }

    fn delete_refresh_token(&self, account: &str) -> Result<()> {
        let entry = Self::entry(account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error)
                .context("Failed to remove the Disc OAuth refresh token from the operating-system credential store."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CredentialStore, SystemCredentialStore};

    #[test]
    fn system_store_round_trips_and_idempotently_deletes_through_the_keyring_contract() {
        let _ = keyring::Entry::new("disc-cli-keyring-initialization", "test");
        keyring_core::set_default_store(
            keyring_core::mock::Store::new().expect("mock credential store"),
        );
        let account = format!("test-{}", uuid::Uuid::new_v4());
        let store = SystemCredentialStore;

        store
            .set_refresh_token(&account, "refresh-secret")
            .expect("store refresh token");
        assert_eq!(
            store
                .get_refresh_token(&account)
                .expect("read refresh token"),
            "refresh-secret"
        );
        store
            .delete_refresh_token(&account)
            .expect("delete refresh token");
        store
            .delete_refresh_token(&account)
            .expect("idempotent delete");
        assert!(store.get_refresh_token(&account).is_err());
    }
}
