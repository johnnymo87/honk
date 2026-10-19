pub mod catalog;
pub mod ops;
pub mod redact;
pub mod wrapper;

use crate::mcp::ops::Policy;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub catalog_path: PathBuf,
    pub policy: Policy,
}
