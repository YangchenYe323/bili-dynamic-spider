use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub mirai: MiraiConfig,
    pub bili: BiliConfig,
    pub target: Vec<TargetConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MiraiConfig {
    pub http_url: String,
    pub verify_key: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BiliConfig {
    pub sess_data: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TargetConfig {
    pub uid: u64,
    pub interval_sec: u64,
    pub receiver_qq: i64,
    pub sender_qq: i64,
}

pub async fn get_config_from_file(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let content = fs::read_to_string(path).await.context("Read config file")?;

    toml::from_str(&content).context("Parse config file")
}
