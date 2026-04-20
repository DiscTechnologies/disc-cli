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
            Some(value) if value.is_empty() == false => value.to_owned(),
            _ => match stored_auth {
                Some(auth) if auth.api_key.is_empty() == false => auth.api_key,
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
            .filter(|value| value.is_empty() == false);

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
        if path.exists() == false {
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
            return Ok(());
        }

        #[cfg(not(unix))]
        {
            fs::write(&path, serde_json::to_string_pretty(value)?)
                .with_context(|| format!("Failed to write {}.", path.display()))?;
            fs::write(&path, format!("{}\n", serde_json::to_string_pretty(value)?))
                .with_context(|| format!("Failed to write {}.", path.display()))?;
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigStore;

    #[test]
    fn discover_returns_non_empty_root_dir() {
        let store = ConfigStore::discover().expect("config store");
        assert!(store.root_dir().as_os_str().is_empty() == false);
    }
}
