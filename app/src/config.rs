use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub input_device: String,
    pub output_device: String,
    pub input_threshold: f32,
    pub output_threshold: f32,
    pub ring_buffer_secs: String,
    pub keepalive_secs: String,
    pub s3: S3Config,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            input_device: String::new(),
            output_device: String::new(),
            input_threshold: 0.15,
            output_threshold: 0.15,
            ring_buffer_secs: "3".into(),
            keepalive_secs: "10".into(),
            s3: S3Config::default(),
        }
    }
}

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("no config dir")?;
        Ok(base.join("audio-friend").join("config.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    pub endpoint_url: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}
