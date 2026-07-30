use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const QUALIFIER: &str = "tech";
const ORGANIZATION: &str = "disctech";
const APPLICATION: &str = "disc";
const AUTH_SCHEMA_VERSION: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredConfig {
    pub http_base_url: Option<String>,
    pub ws_url: Option<String>,
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuthProfile {
    pub profile: String,
    pub api_key: String,
    pub api_base_url: String,
    pub subject_id: Option<String>,
    pub subject_key: Option<String>,
    pub subject_kind: Option<String>,
    pub display_name: Option<String>,
    pub created_at: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub keycloak_user_id: Option<String>,
    #[serde(default)]
    pub credential_store_account: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredAuth {
    pub version: u8,
    pub active_profile: Option<String>,
    pub profiles: BTreeMap<String, StoredAuthProfile>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub api_key: String,
    pub http_base_url: String,
    pub ws_url: String,
    pub client_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    root_dir: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        if let Some(root_dir) =
            std::env::var_os("DISC_CONFIG_DIR").filter(|value| !value.is_empty())
        {
            return Ok(Self {
                root_dir: PathBuf::from(root_dir),
            });
        }
        let project_dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .context("Failed to resolve Disc CLI config directory.")?;
        let root_dir = project_dirs.config_dir().to_path_buf();
        Ok(Self { root_dir })
    }

    pub fn root_dir(&self) -> &PathBuf {
        &self.root_dir
    }

    pub fn credential_lock_path(&self, account: &str) -> Result<PathBuf> {
        self.ensure_dir()?;
        let digest = Sha256::digest(account.as_bytes());
        let name = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        Ok(self.root_dir.join(format!(".oauth-{name}.lock")))
    }

    #[cfg(test)]
    pub(crate) fn at_root(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }

    pub fn load_config(&self) -> Result<StoredConfig> {
        self.read_json::<StoredConfig>("config.json")
            .map(|maybe| maybe.unwrap_or_default())
    }

    pub fn save_config(&self, config: &StoredConfig) -> Result<()> {
        self.write_json("config.json", config)
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>> {
        let path = self.root_dir.join("auth.json");
        if !path.exists() {
            return Ok(None);
        }

        let raw =
            fs::read_to_string(&path).context(format!("Failed to read {}.", path.display()))?;
        let value = serde_json::from_str::<serde_json::Value>(&raw)
            .context(format!("Failed to parse {}.", path.display()))?;

        if value.get("version").is_some() {
            let mut auth = serde_json::from_value::<StoredAuth>(value)
                .context(format!("Failed to parse {}.", path.display()))?;
            if auth.version > AUTH_SCHEMA_VERSION {
                bail!(
                    "Stored Disc credentials use unsupported schema version {} (this CLI supports up to {}).",
                    auth.version,
                    AUTH_SCHEMA_VERSION
                );
            }
            auth.version = AUTH_SCHEMA_VERSION;
            return Ok(Some(auth));
        }

        let api_key = value
            .get("api_key")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .context({
                format!(
                    "Stored legacy Disc credentials in {} are missing api_key.",
                    path.display()
                )
            })?
            .to_owned();
        let profile = StoredAuthProfile {
            profile: "legacy".to_owned(),
            api_key,
            api_base_url: self
                .load_config()?
                .http_base_url
                .unwrap_or_else(|| "https://api.disc.tech".to_owned()),
            subject_id: None,
            subject_key: None,
            subject_kind: None,
            display_name: Some("Legacy API key".to_owned()),
            created_at: None,
            issuer: None,
            oauth_client_id: None,
            keycloak_user_id: None,
            credential_store_account: None,
        };

        Ok(Some(StoredAuth {
            version: AUTH_SCHEMA_VERSION,
            active_profile: Some(profile.profile.clone()),
            profiles: BTreeMap::from([(profile.profile.clone(), profile)]),
        }))
    }

    pub fn save_auth(&self, auth: &StoredAuth) -> Result<()> {
        self.write_json("auth.json", auth)
    }

    pub fn save_profile(&self, profile: StoredAuthProfile) -> Result<()> {
        let mut auth = self.load_auth()?.unwrap_or_else(|| StoredAuth {
            version: AUTH_SCHEMA_VERSION,
            ..StoredAuth::default()
        });
        auth.version = AUTH_SCHEMA_VERSION;
        auth.active_profile = Some(profile.profile.clone());
        auth.profiles.insert(profile.profile.clone(), profile);
        self.save_auth(&auth)
    }

    pub fn use_profile(&self, profile: &str) -> Result<()> {
        let mut auth = self.load_auth()?.context("No stored Disc profiles.")?;
        if !auth.profiles.contains_key(profile) {
            bail!("Disc auth profile `{profile}` was not found.");
        }
        auth.active_profile = Some(profile.to_owned());
        self.save_auth(&auth)
    }

    pub fn clear_active_profile(&self) -> Result<bool> {
        let Some(mut auth) = self.load_auth()? else {
            return Ok(false);
        };
        let Some(active_profile) = auth.active_profile.clone() else {
            return Ok(false);
        };
        let removed = auth.profiles.remove(&active_profile).is_some();
        auth.active_profile = auth.profiles.keys().next().cloned();
        if auth.profiles.is_empty() {
            return self.clear_auth();
        }
        self.save_auth(&auth)?;
        Ok(removed)
    }

    pub fn remove_profile(&self, profile: &str) -> Result<bool> {
        let Some(mut auth) = self.load_auth()? else {
            return Ok(false);
        };
        let removed = auth.profiles.remove(profile).is_some();
        if !removed {
            return Ok(false);
        }
        if auth.active_profile.as_deref() == Some(profile) {
            auth.active_profile = auth.profiles.keys().next().cloned();
        }
        if auth.profiles.is_empty() {
            return self.clear_auth();
        }
        self.save_auth(&auth)?;
        Ok(true)
    }

    pub fn clear_auth(&self) -> Result<bool> {
        let path = self.root_dir.join("auth.json");
        if path.exists() {
            fs::remove_file(&path)
                .context(format!("Failed to remove auth file at {}.", path.display()))?;
            return Ok(true);
        }

        Ok(false)
    }

    #[cfg(test)]
    pub fn resolve(
        &self,
        cli_api_key: Option<&str>,
        cli_http_base_url: Option<&str>,
        cli_ws_url: Option<&str>,
        cli_client_id: Option<&str>,
    ) -> Result<EffectiveConfig> {
        let stored_config = self.load_config()?;
        let stored_auth = self.load_auth()?;
        let active_profile = stored_auth.as_ref().and_then(|auth| {
            auth.active_profile
                .as_ref()
                .and_then(|name| auth.profiles.get(name))
        });

        let api_key = match cli_api_key {
            Some(value) if !value.is_empty() => value.to_owned(),
            _ => match active_profile {
                Some(profile) if !profile.api_key.is_empty() => profile.api_key.clone(),
                _ => bail!(
                    "API key is not configured. Run `disc auth login`, `disc auth api-key set`, or pass `--api-key`."
                ),
            },
        };

        let http_base_url = cli_http_base_url
            .map(str::to_owned)
            .or_else(|| active_profile.map(|profile| profile.api_base_url.clone()))
            .or(stored_config.http_base_url)
            .unwrap_or_else(|| "https://api.disc.tech".to_owned());
        let ws_url = cli_ws_url
            .map(str::to_owned)
            .or(stored_config.ws_url)
            .unwrap_or_else(|| "wss://signals.disc.tech".to_owned());
        let client_id = cli_client_id
            .map(str::to_owned)
            .or(stored_config.client_id)
            .filter(|value| !value.is_empty());

        Ok(EffectiveConfig {
            api_key,
            http_base_url,
            ws_url,
            client_id,
        })
    }

    fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.root_dir).context({
            format!(
                "Failed to create Disc CLI config directory at {}.",
                self.root_dir.display()
            )
        })?;
        Ok(())
    }

    fn read_json<T>(&self, name: &str) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let path = self.root_dir.join(name);
        if !path.exists() {
            return Ok(None);
        }

        let raw =
            fs::read_to_string(&path).context(format!("Failed to read {}.", path.display()))?;
        let parsed = serde_json::from_str::<T>(&raw)
            .context(format!("Failed to parse {}.", path.display()))?;
        Ok(Some(parsed))
    }

    fn write_json<T>(&self, name: &str, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        self.ensure_dir()?;

        let path = self.root_dir.join(name);
        let json = serde_json::to_vec_pretty(value)
            .context(format!("Failed to serialize {}.", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            let temporary_path = self
                .root_dir
                .join(format!(".{name}.{}.tmp", std::process::id()));
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary_path)
                .context({
                    format!(
                        "Failed to open temporary credential file in {}.",
                        self.root_dir.display()
                    )
                })?;
            file.write_all(&json)
                .context(format!("Failed to write {}.", temporary_path.display()))?;
            file.write_all(b"\n")
                .context(format!("Failed to finalize {}.", temporary_path.display()))?;
            file.sync_all()
                .context(format!("Failed to sync {}.", temporary_path.display()))?;
            fs::rename(&temporary_path, &path).context({
                format!(
                    "Failed to atomically replace credentials at {}.",
                    path.display()
                )
            })?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let serialized = serde_json::to_string_pretty(value)
                .context(format!("Failed to serialize {}.", path.display()))?;
            fs::write(&path, format!("{serialized}\n"))
                .context(format!("Failed to write {}.", path.display()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{AUTH_SCHEMA_VERSION, ConfigStore, StoredAuth, StoredAuthProfile, StoredConfig};

    fn stored_auth(api_key: &str) -> StoredAuth {
        let profile = StoredAuthProfile {
            profile: "test".to_owned(),
            api_key: api_key.to_owned(),
            api_base_url: "https://api.disc.tech".to_owned(),
            subject_id: None,
            subject_key: None,
            subject_kind: None,
            display_name: None,
            created_at: None,
            issuer: None,
            oauth_client_id: None,
            keycloak_user_id: None,
            credential_store_account: None,
        };
        StoredAuth {
            version: AUTH_SCHEMA_VERSION,
            active_profile: Some(profile.profile.clone()),
            profiles: [(profile.profile.clone(), profile)].into(),
        }
    }

    fn temporary_store(test_name: &str) -> ConfigStore {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        ConfigStore {
            root_dir: std::env::temp_dir().join(format!(
                "disc-cli-config-{test_name}-{}-{nonce}",
                std::process::id()
            )),
        }
    }

    fn remove_store(store: &ConfigStore) {
        if store.root_dir.exists() {
            fs::remove_dir_all(&store.root_dir).expect("remove temporary config directory");
        }
    }

    #[test]
    fn discover_returns_non_empty_root_dir() {
        let store = ConfigStore::discover().expect("config store");
        assert!(!store.root_dir().as_os_str().is_empty());
    }

    #[test]
    fn config_and_auth_round_trip_and_clear_cleanly() {
        let store = temporary_store("round-trip");
        let config = StoredConfig {
            http_base_url: Some("https://api.example.test".to_owned()),
            ws_url: Some("wss://signals.example.test".to_owned()),
            client_id: Some("client-one".to_owned()),
        };
        let auth = stored_auth("secret-key");

        assert!(!store.clear_auth().expect("clear absent auth"));
        store.save_config(&config).expect("save config");
        store.save_auth(&auth).expect("save auth");

        let loaded_config = store.load_config().expect("load config");
        assert_eq!(
            loaded_config.http_base_url.as_deref(),
            Some("https://api.example.test")
        );
        assert_eq!(
            loaded_config.ws_url.as_deref(),
            Some("wss://signals.example.test")
        );
        assert_eq!(loaded_config.client_id.as_deref(), Some("client-one"));
        assert_eq!(
            store
                .load_auth()
                .expect("load auth")
                .expect("stored auth")
                .profiles["test"]
                .api_key,
            "secret-key"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(store.root_dir.join("auth.json"))
                .expect("auth metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        assert!(store.clear_auth().expect("clear auth"));
        assert!(store.load_auth().expect("load cleared auth").is_none());
        remove_store(&store);
    }

    #[test]
    fn missing_config_uses_defaults_and_cli_values_take_precedence() {
        let store = temporary_store("precedence");
        store
            .save_auth(&stored_auth("stored-key"))
            .expect("save auth");
        store
            .save_config(&StoredConfig {
                http_base_url: Some("https://stored.example.test".to_owned()),
                ws_url: Some("wss://stored.example.test".to_owned()),
                client_id: Some("stored-client".to_owned()),
            })
            .expect("save config");
        let mut auth = store.load_auth().expect("load auth").expect("stored auth");
        auth.profiles
            .get_mut("test")
            .expect("test profile")
            .api_base_url = "https://stored.example.test".to_owned();
        store.save_auth(&auth).expect("save endpoint-bound auth");

        let stored = store
            .resolve(None, None, None, None)
            .expect("resolve stored config");
        assert_eq!(stored.api_key, "stored-key");
        assert_eq!(stored.http_base_url, "https://stored.example.test");
        assert_eq!(stored.ws_url, "wss://stored.example.test");
        assert_eq!(stored.client_id.as_deref(), Some("stored-client"));

        let overridden = store
            .resolve(
                Some("cli-key"),
                Some("https://cli.example.test"),
                Some("wss://cli.example.test"),
                Some("cli-client"),
            )
            .expect("resolve CLI config");
        assert_eq!(overridden.api_key, "cli-key");
        assert_eq!(overridden.http_base_url, "https://cli.example.test");
        assert_eq!(overridden.ws_url, "wss://cli.example.test");
        assert_eq!(overridden.client_id.as_deref(), Some("cli-client"));
        remove_store(&store);
    }

    #[test]
    fn defaults_apply_without_stored_endpoints_and_blank_client_id_is_removed() {
        let store = temporary_store("defaults");
        store
            .save_auth(&stored_auth("stored-key"))
            .expect("save auth");

        let resolved = store
            .resolve(None, None, None, Some(""))
            .expect("resolve default endpoints");
        assert_eq!(resolved.http_base_url, "https://api.disc.tech");
        assert_eq!(resolved.ws_url, "wss://signals.disc.tech");
        assert!(resolved.client_id.is_none());
        remove_store(&store);
    }

    #[test]
    fn resolve_rejects_missing_or_blank_api_keys() {
        let store = temporary_store("missing-key");
        let error = store
            .resolve(None, None, None, None)
            .expect_err("missing API key must fail");
        assert!(error.to_string().contains("API key is not configured"));

        store.save_auth(&stored_auth("")).expect("save blank auth");
        let error = store
            .resolve(Some(""), None, None, None)
            .expect_err("blank API keys must fail");
        assert!(error.to_string().contains("API key is not configured"));
        remove_store(&store);
    }

    #[test]
    fn malformed_config_and_auth_files_report_their_paths() {
        let store = temporary_store("malformed");
        fs::create_dir_all(&store.root_dir).expect("create config directory");
        fs::write(store.root_dir.join("config.json"), "{not json").expect("write malformed config");
        let config_error = store.load_config().expect_err("malformed config must fail");
        assert!(config_error.to_string().contains("config.json"));

        fs::remove_file(store.root_dir.join("config.json")).expect("remove config");
        fs::write(store.root_dir.join("auth.json"), "[]").expect("write malformed auth");
        let auth_error = store.load_auth().expect_err("malformed auth must fail");
        assert!(auth_error.to_string().contains("auth.json"));
        remove_store(&store);
    }

    #[test]
    fn legacy_single_key_auth_is_preserved_as_an_unresolved_profile() {
        let store = temporary_store("legacy-auth");
        fs::create_dir_all(&store.root_dir).expect("create config directory");
        store
            .save_config(&StoredConfig {
                http_base_url: Some("https://legacy.example.test".to_owned()),
                ..StoredConfig::default()
            })
            .expect("save legacy endpoint");
        fs::write(
            store.root_dir.join("auth.json"),
            r#"{"api_key":"legacy-secret"}"#,
        )
        .expect("write legacy auth");

        let auth = store.load_auth().expect("load legacy auth").expect("auth");
        assert_eq!(auth.version, AUTH_SCHEMA_VERSION);
        assert_eq!(auth.active_profile.as_deref(), Some("legacy"));
        assert_eq!(auth.profiles["legacy"].api_key, "legacy-secret");
        assert_eq!(
            auth.profiles["legacy"].api_base_url,
            "https://legacy.example.test"
        );
        assert!(auth.profiles["legacy"].subject_id.is_none());
        remove_store(&store);
    }

    #[test]
    fn version_two_profiles_migrate_without_inventing_oauth_credentials() {
        let store = temporary_store("v2-migration");
        fs::create_dir_all(&store.root_dir).expect("create config directory");
        fs::write(
            store.root_dir.join("auth.json"),
            r#"{
              "version": 2,
              "active_profile": "partner",
              "profiles": {
                "partner": {
                  "profile": "partner",
                  "api_key": "legacy-api-key",
                  "api_base_url": "https://api.disc.tech",
                  "subject_id": "42",
                  "subject_key": "partner",
                  "subject_kind": "partner",
                  "display_name": "Partner",
                  "created_at": "2026-07-30T00:00:00Z"
                }
              }
            }"#,
        )
        .expect("write v2 auth");

        let auth = store.load_auth().expect("load v2 auth").expect("auth");
        assert_eq!(auth.version, AUTH_SCHEMA_VERSION);
        let profile = &auth.profiles["partner"];
        assert_eq!(profile.api_key, "legacy-api-key");
        assert!(profile.issuer.is_none());
        assert!(profile.oauth_client_id.is_none());
        assert!(profile.credential_store_account.is_none());
        remove_store(&store);
    }

    #[test]
    fn future_auth_schema_is_rejected_without_rewriting_the_file() {
        let store = temporary_store("future-schema");
        fs::create_dir_all(&store.root_dir).expect("create config directory");
        let path = store.root_dir.join("auth.json");
        let original = r#"{"version":255,"active_profile":null,"profiles":{}}"#;
        fs::write(&path, original).expect("write future auth");

        let error = store.load_auth().expect_err("future schema must fail");
        assert!(error.to_string().contains("unsupported schema version 255"));
        assert_eq!(
            fs::read_to_string(path).expect("read unchanged auth"),
            original
        );
        remove_store(&store);
    }

    #[test]
    fn credential_lock_names_are_deterministic_and_do_not_expose_account_ids() {
        let store = temporary_store("credential-lock");
        let first = store
            .credential_lock_path("issuer:user:subject")
            .expect("lock path");
        let second = store
            .credential_lock_path("issuer:user:subject")
            .expect("lock path");
        let other = store
            .credential_lock_path("issuer:user:other-subject")
            .expect("other lock path");
        assert_eq!(first, second);
        assert_ne!(first, other);
        let name = first
            .file_name()
            .and_then(|value| value.to_str())
            .expect("lock filename");
        assert!(name.starts_with(".oauth-"));
        assert!(!name.contains("issuer"));
        assert!(!name.contains("subject"));
        remove_store(&store);
    }

    #[test]
    fn profile_switch_and_targeted_clear_preserve_unrelated_subjects() {
        let store = temporary_store("profiles");
        let mut first = stored_auth("first-key")
            .profiles
            .remove("test")
            .expect("profile");
        first.profile = "first".to_owned();
        let mut second = first.clone();
        second.profile = "second".to_owned();
        second.api_key = "second-key".to_owned();

        store.save_profile(first).expect("save first");
        store.save_profile(second).expect("save second");
        store.use_profile("first").expect("select first");
        assert_eq!(
            store
                .resolve(None, None, None, None)
                .expect("resolve")
                .api_key,
            "first-key"
        );
        assert!(store.clear_active_profile().expect("clear active"));

        let auth = store.load_auth().expect("load auth").expect("auth");
        assert_eq!(auth.active_profile.as_deref(), Some("second"));
        assert!(!auth.profiles.contains_key("first"));
        assert_eq!(auth.profiles["second"].api_key, "second-key");
        remove_store(&store);
    }

    #[test]
    fn named_profile_removal_preserves_failures_and_selects_a_remaining_profile() {
        let store = temporary_store("named-removal");
        let mut first = stored_auth("first-key")
            .profiles
            .remove("test")
            .expect("profile");
        first.profile = "first".to_owned();
        let mut second = first.clone();
        second.profile = "second".to_owned();
        store.save_profile(first).expect("save first");
        store.save_profile(second).expect("save second");

        assert!(store.remove_profile("second").expect("remove active"));
        let auth = store.load_auth().expect("load auth").expect("auth");
        assert_eq!(auth.active_profile.as_deref(), Some("first"));
        assert!(auth.profiles.contains_key("first"));
        assert!(!store.remove_profile("missing").expect("remove missing"));
        remove_store(&store);
    }

    #[test]
    fn unreadable_config_path_reports_read_failure() {
        let store = temporary_store("unreadable");
        fs::create_dir_all(store.root_dir.join("config.json")).expect("create directory at path");

        let error = store
            .load_config()
            .expect_err("directory cannot be read as config file");
        assert!(error.to_string().contains("Failed to read"));
        remove_store(&store);
    }

    #[test]
    fn root_dir_returns_the_config_location() {
        let root_dir = PathBuf::from("/tmp/disc-cli-config-location");
        let store = ConfigStore {
            root_dir: root_dir.clone(),
        };
        assert_eq!(store.root_dir(), &root_dir);
    }
}
