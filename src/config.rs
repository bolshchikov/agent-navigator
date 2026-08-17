use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::envelope::CapabilityTier;

pub const USER_AGENT: &str =
    "AgentNavigator/0.1.0 (+https://github.com/bolshchikov/agent-navigator; agent-native HTTP client; no JavaScript)";

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_REDIRECT_CAP: usize = 10;
pub const DEFAULT_MAX_RETRIES: u32 = 3;
pub const DEFAULT_DISCOVERY_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub user_agent: String,
    pub timeout: Duration,
    pub redirect_cap: usize,
    pub max_retries: u32,
    pub discovery_ttl: Duration,
    pub max_body_bytes: u64,
    pub ignore_robots: bool,
    /// Per-host classification overrides (FR17). Host is lowercase, no port if default.
    pub tier_overrides: BTreeMap<String, CapabilityTier>,
    pub session_dir: PathBuf,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            user_agent: USER_AGENT.to_string(),
            timeout: DEFAULT_TIMEOUT,
            redirect_cap: DEFAULT_REDIRECT_CAP,
            max_retries: DEFAULT_MAX_RETRIES,
            discovery_ttl: DEFAULT_DISCOVERY_TTL,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            ignore_robots: false,
            tier_overrides: BTreeMap::new(),
            session_dir: default_session_dir(),
        }
    }
}

impl ClientConfig {
    pub fn override_for(&self, host: &str) -> Option<CapabilityTier> {
        let host = host.to_ascii_lowercase();
        self.tier_overrides
            .get(&host)
            .copied()
            .or_else(|| self.tier_overrides.get(&strip_www(&host)).copied())
    }
}

fn strip_www(host: &str) -> String {
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

pub fn default_session_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-navigator")
        .join("sessions")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverrideFile {
    /// Map of hostname → capability tier (`structured`, `static-readable`, `js-required`).
    #[serde(default)]
    pub domains: BTreeMap<String, String>,
}

impl OverrideFile {
    pub fn into_tiers(self) -> BTreeMap<String, CapabilityTier> {
        self.domains
            .into_iter()
            .filter_map(|(host, tier)| parse_tier(&tier).map(|t| (host.to_ascii_lowercase(), t)))
            .collect()
    }
}

pub fn parse_tier(s: &str) -> Option<CapabilityTier> {
    match s.trim().to_ascii_lowercase().as_str() {
        "structured" => Some(CapabilityTier::Structured),
        "static-readable" | "static_readable" | "static" => Some(CapabilityTier::StaticReadable),
        "js-required" | "js_required" | "js" => Some(CapabilityTier::JsRequired),
        _ => None,
    }
}
