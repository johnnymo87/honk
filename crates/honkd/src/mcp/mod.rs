pub mod catalog;
pub mod ops;
pub mod redact;
pub mod wrapper;

use crate::mcp::ops::Policy;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmMcpConfig {
    pub bin: PathBuf,
    pub pigeon_url: String,
    pub pigeon_token_file: Option<PathBuf>,
    pub honk_url: String,
    pub honk_secret_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub catalog_path: Option<PathBuf>,
    pub policy: Policy,
    pub swarm: Option<SwarmMcpConfig>,
}
