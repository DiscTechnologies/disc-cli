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
    pub(crate) fn from_root(root_dir: PathBuf) -> Self {
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
                .context(format!("Failed to remove auth file at {}.", path.display()))?;
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
        fs::create_dir_all(&self.root_dir).context(format!(
            "Failed to create Disc CLI config directory at {}.",
            self.root_dir.display()
        ))?;
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

            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .context(format!("Failed to open {} for writing.", path.display()))?;
            file.write_all(&json)
                .context(format!("Failed to write {}.", path.display()))?;
            file.write_all(b"\n")
                .context(format!("Failed to finalize {}.", path.display()))?;
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

    use super::{ConfigStore, StoredAuth, StoredConfig};

    fn test_store(name: &str) -> ConfigStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root_dir = std::env::temp_dir().join(format!(
            "disc-cli-config-{name}-{}-{unique}",
            std::process::id()
        ));
        ConfigStore { root_dir }
    }

    fn cleanup(store: &ConfigStore) {
        if store.root_dir.exists() {
            fs::remove_dir_all(&store.root_dir).expect("remove test config");
        }
    }

    #[test]
    fn discover_returns_non_empty_root_dir() {
        let store = ConfigStore::discover().expect("config store");
        assert!(!store.root_dir().as_os_str().is_empty());
    }

    #[test]
    fn config_and_auth_round_trip_with_private_files() {
        let store = test_store("round-trip");
        let config = StoredConfig {
            http_base_url: Some("http://localhost:8080".to_owned()),
            ws_url: Some("ws://localhost:8081".to_owned()),
            client_id: Some("client".to_owned()),
        };
        let auth = StoredAuth {
            api_key: "secret".to_owned(),
        };

        assert_eq!(
            store.load_config().expect("empty config").http_base_url,
            None
        );
        assert!(store.load_auth().expect("empty auth").is_none());
        store.save_config(&config).expect("save config");
        store.save_auth(&auth).expect("save auth");

        let loaded_config = store.load_config().expect("load config");
        let loaded_auth = store.load_auth().expect("load auth").expect("stored auth");
        assert_eq!(
            loaded_config.http_base_url.as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(loaded_auth.api_key, "secret");

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

        assert!(store.clear_auth().expect("clear existing auth"));
        assert!(!store.clear_auth().expect("clear missing auth"));
        cleanup(&store);
    }

    #[test]
    fn resolve_honours_cli_stored_and_default_precedence() {
        let store = test_store("resolve");
        store
            .save_config(&StoredConfig {
                http_base_url: Some("https://stored-http".to_owned()),
                ws_url: Some("wss://stored-ws".to_owned()),
                client_id: Some("stored-client".to_owned()),
            })
            .expect("save config");
        store
            .save_auth(&StoredAuth {
                api_key: "stored-key".to_owned(),
            })
            .expect("save auth");

        let stored = store
            .resolve(None, None, None, None)
            .expect("resolve stored");
        assert_eq!(stored.api_key, "stored-key");
        assert_eq!(stored.http_base_url, "https://stored-http");
        assert_eq!(stored.ws_url, "wss://stored-ws");
        assert_eq!(stored.client_id.as_deref(), Some("stored-client"));

        let cli = store
            .resolve(
                Some("cli-key"),
                Some("https://cli-http"),
                Some("wss://cli-ws"),
                Some(""),
            )
            .expect("resolve cli");
        assert_eq!(cli.api_key, "cli-key");
        assert_eq!(cli.http_base_url, "https://cli-http");
        assert_eq!(cli.ws_url, "wss://cli-ws");
        assert!(cli.client_id.is_none());

        store
            .save_config(&StoredConfig::default())
            .expect("reset config");
        let defaults = store
            .resolve(Some("key"), None, None, None)
            .expect("resolve defaults");
        assert_eq!(defaults.http_base_url, "https://api.disc.tech");
        assert_eq!(defaults.ws_url, "wss://signals.disc.tech");
        cleanup(&store);
    }

    #[test]
    fn resolve_requires_a_non_empty_api_key() {
        let store = test_store("missing-key");
        store
            .save_auth(&StoredAuth {
                api_key: String::new(),
            })
            .expect("save empty auth");

        let error = store
            .resolve(Some(""), None, None, None)
            .expect_err("missing key should fail");
        assert!(error.to_string().contains("API key is not configured"));
        cleanup(&store);
    }

    #[test]
    fn malformed_config_reports_its_path() {
        let store = test_store("malformed");
        fs::create_dir_all(&store.root_dir).expect("create config dir");
        let path = store.root_dir.join("config.json");
        fs::write(&path, "{").expect("write malformed config");

        let error = store.load_config().expect_err("malformed config");
        assert!(error.to_string().contains("Failed to parse"));
        assert!(error.to_string().contains("config.json"));
        cleanup(&store);
    }

    #[test]
    fn read_and_write_failures_include_context() {
        let store = test_store("io-errors");
        fs::create_dir_all(&store.root_dir).expect("create root");
        let config_path = store.root_dir.join("config.json");
        fs::create_dir(&config_path).expect("create path as directory");

        let read_error = store.load_config().expect_err("directory read should fail");
        assert!(read_error.to_string().contains("Failed to read"));

        let auth_path: PathBuf = store.root_dir.join("auth.json");
        fs::create_dir(&auth_path).expect("create auth path as directory");
        let write_error = store
            .save_auth(&StoredAuth {
                api_key: "key".to_owned(),
            })
            .expect_err("directory open should fail");
        assert!(write_error.to_string().contains("Failed to open"));
        cleanup(&store);
    }

    #[test]
    fn directory_creation_failures_include_the_config_path() {
        let parent = test_store("directory-error");
        fs::write(&parent.root_dir, "not a directory").expect("create blocking file");
        let store = ConfigStore::from_root(parent.root_dir.join("child"));

        let error = store
            .save_config(&StoredConfig::default())
            .expect_err("directory creation should fail");

        assert!(error.to_string().contains("Failed to create"));
        assert!(error.to_string().contains("child"));
        fs::remove_file(&parent.root_dir).expect("remove blocking file");
    }
}
