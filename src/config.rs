use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub as_token: String,
    pub hs_token: String,
    pub user_id: String,
    pub homeserver: String,
    pub server_name: String,
    pub discord_token: String,
    pub port: u16,
    pub database: PathBuf,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub fn create_default(path: impl AsRef<Path>) -> anyhow::Result<()> {
        let default = Self {
            as_token: "my-secret-as-token".to_string(),
            hs_token: "my-secret-hs-token".to_string(),
            user_id: "appservice-discord".to_string(),
            homeserver: "http://127.0.0.1:8008".to_string(),
            server_name: "localhost".to_string(),
            discord_token: "my-secret-discord-token".to_string(),
            port: 5000,
            database: PathBuf::from("bridge.db"),
        };

        let content = serde_json::to_string_pretty(&default)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn full_user_id(&self) -> String {
        format!("@{}:{}", self.user_id, self.server_name)
    }
}
