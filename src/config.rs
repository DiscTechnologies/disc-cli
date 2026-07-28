use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const QUALIFIER: &str = "tech";
const ORGANIZATION: &str = "disctech";
const APPLICATION: &str = "disc";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredConfig {
    pub http_base_url: Option<String>,
    pub ws_url: Option<String>,
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuth {
    pub api_key: String,
}

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
        let project_dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .context("Failed to resolve Disc CLI config directory.")?;
        let root_dir = project_dirs.config_dir().to_path_buf();
        Ok(Self { root_dir })
    }

    pub fn root_dir(&self) -> &PathBuf {
        &self.root_dir
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
        self.read_json::<StoredAuth>("auth.json")
    }

    pub fn save_auth(&self, auth: &StoredAuth) -> Result<()> {
        self.write_json("auth.json", auth)
    }

    pub fn clear_auth(&self) -> Result<bool> {
        let path = self.root_dir.join("auth.json");
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove auth file at {}.", path.display()))?;
            return Ok(true);
        }

        Ok(false)
    }

    pub fn resolve(
        &self,
        cli_api_key: Option<&str>,
        cli_http_base_url: Option<&str>,
        cli_ws_url: Option<&str>,
        cli_client_id: Option<&str>,
    ) -> Result<EffectiveConfig> {
        let stored_config = self.load_config()?;
        let stored_auth = self.load_auth()?;

        let api_key = match cli_api_key {
            Some(value) if !value.is_empty() => value.to_owned(),
            _ => match stored_auth {
                Some(auth) if !auth.api_key.is_empty() => auth.api_key,
                _ => bail!(
                    "API key is not configured. Run `disc auth api-key set` or pass `--api-key`."
                ),
            },
        };

        let http_base_url = cli_http_base_url
            .map(str::to_owned)
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
        fs::create_dir_all(&self.root_dir).with_context(|| {
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

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}.", path.display()))?;
        let parsed = serde_json::from_str::<T>(&raw)
            .with_context(|| format!("Failed to parse {}.", path.display()))?;
        Ok(Some(parsed))
    }

    fn write_json<T>(&self, name: &str, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        self.ensure_dir()?;

        let path = self.root_dir.join(name);
        let json = serde_json::to_vec_pretty(value)
            .with_context(|| format!("Failed to serialize {}.", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("Failed to open {} for writing.", path.display()))?;
            file.write_all(&json)
                .with_context(|| format!("Failed to write {}.", path.display()))?;
            file.write_all(b"\n")
                .with_context(|| format!("Failed to finalize {}.", path.display()))?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let serialized = serde_json::to_string_pretty(value)
                .with_context(|| format!("Failed to serialize {}.", path.display()))?;
            fs::write(&path, format!("{serialized}\n"))
                .with_context(|| format!("Failed to write {}.", path.display()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ConfigStore, StoredAuth, StoredConfig};

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
        let auth = StoredAuth {
            api_key: "secret-key".to_owned(),
        };

        assert_eq!(store.clear_auth().expect("clear absent auth"), false);
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

        assert_eq!(store.clear_auth().expect("clear auth"), true);
        assert!(store.load_auth().expect("load cleared auth").is_none());
        remove_store(&store);
    }

    #[test]
    fn missing_config_uses_defaults_and_cli_values_take_precedence() {
        let store = temporary_store("precedence");
        store
            .save_auth(&StoredAuth {
                api_key: "stored-key".to_owned(),
            })
            .expect("save auth");
        store
            .save_config(&StoredConfig {
                http_base_url: Some("https://stored.example.test".to_owned()),
                ws_url: Some("wss://stored.example.test".to_owned()),
                client_id: Some("stored-client".to_owned()),
            })
            .expect("save config");

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
            .save_auth(&StoredAuth {
                api_key: "stored-key".to_owned(),
            })
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

        store
            .save_auth(&StoredAuth {
                api_key: String::new(),
            })
            .expect("save blank auth");
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
