use anyhow::Context;
use serde::de;
use serde::{Deserialize, Deserializer};
use std::fs;
use std::path::Path;

/// Custom deserializer for llama_server_args that parses a shell-style string.
/// Example: "-fa on --no-kv-offload" becomes vec!["-fa", "on", "--no-kv-offload"]
fn deserialize_llama_server_args<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        return Ok(vec![]);
    }
    shlex::split(&s).ok_or_else(|| {
        de::Error::custom(format!(
            "Invalid shell syntax in llama_server_args: '{}'. Check for unmatched quotes.",
            s
        ))
    })
}

#[derive(Debug, Deserialize, Clone)]
pub struct NodeConfig {
    pub node_id: String,
    pub listen_addr: String,
    pub public_url: Option<String>,
    pub max_vram_mb: u64,
    pub max_sysmem_mb: u64,
    #[serde(default = "default_max_instances_per_node")]
    pub max_instances_per_node: usize,
    #[serde(default = "default_metrics_path")]
    pub metrics_path: String,
    pub default_model: String,
    pub model_defaults: ModelDefaults,
    pub llama_cpp_ports: Option<LlamaCppPorts>,
    pub llama_cpp: LlamaCppConfig,
    pub cluster: ClusterConfig,
    pub http: HttpConfig,
    pub auth: Option<AuthConfig>,
    pub server_tls: Option<ServerTlsConfig>,
    pub cluster_tls: Option<ClusterTlsConfig>,
    #[serde(default = "default_shutdown_grace_period_seconds")]
    pub shutdown_grace_period_seconds: u64,
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    #[serde(default)]
    pub logging: Option<LoggingConfig>,
    /// Global limit on total queue entries across all models.
    /// Prevents unbounded memory growth under sustained overload.
    /// 0 = no global limit (only per-model limits apply).
    #[serde(default = "default_max_total_queue_entries")]
    pub max_total_queue_entries: usize,
}

fn default_shutdown_grace_period_seconds() -> u64 {
    30
}

fn default_max_hops() -> usize {
    10
}

fn default_max_total_queue_entries() -> usize {
    0 // 0 = no global limit (only per-model limits apply)
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecretsConfig {
    pub auth: Option<AuthConfig>,
    pub server_tls: Option<ServerTlsConfig>,
    pub cluster_tls: Option<ClusterTlsConfig>,
}

impl NodeConfig {
    pub fn merge_secrets(&mut self, secrets: SecretsConfig) {
        if let Some(auth) = secrets.auth {
            self.auth = Some(auth);
        }
        if let Some(server_tls) = secrets.server_tls {
            self.server_tls = Some(server_tls);
        }
        if let Some(cluster_tls) = secrets.cluster_tls {
            self.cluster_tls = Some(cluster_tls);
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        // Deprecation warning for cluster_tls when noise is enabled
        if let Some(cluster_tls) = &self.cluster_tls {
            if cluster_tls.enabled {
                if self.cluster.noise.enabled {
                    tracing::warn!(
                        "DEPRECATION: cluster_tls is deprecated when noise is enabled. \
                         Both are configured - noise will be preferred for new connections. \
                         Remove cluster_tls configuration to silence this warning."
                    );
                }
                if let Some(server_tls) = &self.server_tls {
                    if !server_tls.enabled {
                        return Err(anyhow::anyhow!("Cluster TLS is enabled but Server TLS is disabled. Server TLS is required for mTLS to work."));
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "Cluster TLS is enabled but Server TLS is not configured."
                    ));
                }
            }
        }
        Ok(())
    }
}

fn default_max_instances_per_node() -> usize {
    100
}
fn default_metrics_path() -> String {
    "./node-metrics.json".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_log_directory")]
    pub directory: String,
    #[serde(default = "default_log_filename")]
    pub filename: String,
    #[serde(default = "default_max_keep_files")]
    pub max_keep_files: u64,
    #[serde(default = "default_compression")]
    pub compression: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: default_log_directory(),
            filename: default_log_filename(),
            max_keep_files: default_max_keep_files(),
            compression: default_compression(),
        }
    }
}

fn default_log_directory() -> String {
    "./logs".into()
}
fn default_log_filename() -> String {
    "proxy.log".into()
}
fn default_max_keep_files() -> u64 {
    7
}
fn default_compression() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct ModelDefaults {
    pub max_concurrent_requests_per_instance: usize,
    /// Maximum queue size per model. 0 = unlimited (wait forever for capacity).
    pub max_queue_size_per_model: usize,
    pub max_instances_per_model: usize,
    /// Maximum time (ms) to wait in queue. 0 = infinite wait (never timeout).
    #[serde(default = "default_max_wait_in_queue_ms")]
    pub max_wait_in_queue_ms: u64,
    /// Maximum request duration (ms). 0 = unlimited (no timeout).
    #[serde(default = "default_max_request_duration_ms")]
    pub max_request_duration_ms: u64,
}

fn default_max_wait_in_queue_ms() -> u64 {
    60_000
}

fn default_max_request_duration_ms() -> u64 {
    0
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlamaCppPorts {
    pub ports: Option<Vec<u16>>,
    pub ranges: Option<Vec<PortRange>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlamaCppConfig {
    pub repo_url: String,
    #[serde(default = "default_repo_path")]
    pub repo_path: String,
    #[serde(default = "default_build_path")]
    pub build_path: String,
    #[serde(default = "default_binary_path")]
    pub binary_path: String,
    pub branch: String,
    #[serde(default = "default_build_args")]
    pub build_args: Vec<String>,
    #[serde(default)]
    pub build_command_args: Vec<String>,
    pub auto_update_interval_seconds: u64,
    #[serde(default = "default_build_enabled")]
    pub enabled: bool,
    #[serde(default = "default_keep_builds")]
    pub keep_builds: usize,
}

fn default_keep_builds() -> usize {
    3
}

fn default_build_enabled() -> bool {
    true
}

fn default_repo_path() -> String {
    "./llama.cpp".to_string()
}
fn default_build_path() -> String {
    "./llama.cpp/build".to_string()
}
fn default_binary_path() -> String {
    "./llama.cpp/bin/llama-server".to_string()
}

fn default_build_args() -> Vec<String> {
    vec![
        "-DGGML_CUDA=ON".to_string(),
        "-DGGML_CUDA_FA_ALL_QUANTS=ON".to_string(),
    ]
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClusterConfig {
    pub enabled: bool,
    /// Explicit peer addresses for WAN/cross-subnet. Additive with mDNS discovery.
    #[serde(default)]
    pub peers: Vec<String>,
    pub gossip_interval_seconds: u64,
    #[serde(default = "default_max_concurrent_gossip")]
    pub max_concurrent_gossip: usize,
    /// Discovery configuration (mDNS, etc.)
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Noise Protocol configuration for inter-node encryption
    #[serde(default)]
    pub noise: ClusterNoiseConfig,
    /// Circuit breaker configuration for peer connections
    #[serde(default)]
    pub circuit_breaker: crate::circuit_breaker::CircuitBreakerConfig,
    /// How to handle version mismatches with peers.
    /// "warn" = log warning but accept (default)
    /// "reject_major" = reject peers with different major version
    /// "reject_any" = reject peers with any version difference
    #[serde(default = "default_version_mismatch_action")]
    pub version_mismatch_action: String,
}

fn default_version_mismatch_action() -> String {
    "warn".to_string()
}

fn default_max_concurrent_gossip() -> usize {
    16
}

#[derive(Debug, Deserialize, Clone)]
pub struct DiscoveryConfig {
    /// Enable mDNS auto-discovery for LAN peers. Default: true
    #[serde(default = "default_true")]
    pub mdns: bool,
    /// mDNS service name. Default: "_llama-mesh._tcp.local"
    #[serde(default = "default_mdns_service_name")]
    pub service_name: String,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mdns: true,
            service_name: default_mdns_service_name(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_mdns_service_name() -> String {
    "_llama-mesh._tcp.local".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClusterNoiseConfig {
    /// Enable Noise Protocol encryption. Default: true
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Enable TOFU (Trust-On-First-Use). Default: true (hobbyist-friendly)
    #[serde(default = "default_true")]
    pub tofu: bool,
    /// Config directory for keys/tokens. Default: ~/.llama-mesh/
    #[serde(default)]
    pub config_dir: Option<String>,
    /// Path to private key file. Default: config_dir/node.key
    #[serde(default)]
    pub private_key_path: Option<String>,
    /// Path to previous private key for rotation. Default: none
    #[serde(default)]
    #[allow(dead_code)] // Key rotation not yet implemented
    pub previous_key_path: Option<String>,
    /// Path to known peers file. Default: config_dir/known_peers
    #[serde(default)]
    pub known_peers_path: Option<String>,
    /// Enterprise: list of allowed public keys. If non-empty, only these keys accepted.
    #[serde(default)]
    pub allowed_keys: Vec<String>,
    /// Session TTL in seconds. Default: 3600 (1 hour)
    #[serde(default = "default_session_ttl")]
    #[allow(dead_code)] // Reserved for Noise session caching
    pub session_ttl_seconds: u64,
}

impl Default for ClusterNoiseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tofu: true,
            config_dir: None,
            private_key_path: None,
            previous_key_path: None,
            known_peers_path: None,
            allowed_keys: vec![],
            session_ttl_seconds: default_session_ttl(),
        }
    }
}

fn default_session_ttl() -> u64 {
    3600
}

impl ClusterNoiseConfig {
    /// Get effective config directory path
    pub fn effective_config_dir(&self) -> std::path::PathBuf {
        self.config_dir
            .as_ref()
            .map(|s| {
                if s.starts_with("~/") {
                    dirs::home_dir()
                        .map(|h| h.join(&s[2..]))
                        .unwrap_or_else(|| {
                            tracing::warn!(
                                "Could not expand ~ in config path; using path as literal: {}",
                                s
                            );
                            std::path::PathBuf::from(s)
                        })
                } else {
                    std::path::PathBuf::from(s)
                }
            })
            .unwrap_or_else(crate::noise::default_config_dir)
    }

    /// Get effective private key path
    pub fn effective_private_key_path(&self, config_dir: &std::path::Path) -> std::path::PathBuf {
        self.private_key_path
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config_dir.join("node.key"))
    }

    /// Get effective known peers path
    pub fn effective_known_peers_path(&self, config_dir: &std::path::Path) -> std::path::PathBuf {
        self.known_peers_path
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config_dir.join("known_peers"))
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct HttpConfig {
    pub request_body_limit_bytes: usize,
    pub idle_timeout_seconds: u64,
    /// Timeout for reading request body in milliseconds (default: 30000 = 30s).
    /// Protects against slowloris-style DoS attacks.
    #[serde(default = "default_body_read_timeout_ms")]
    pub body_read_timeout_ms: u64,
    /// Timeout for protocol detection in milliseconds (default: 10000 = 10s).
    /// Protects against connection exhaustion attacks.
    #[serde(default = "default_protocol_detect_timeout_ms")]
    pub protocol_detect_timeout_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub required_header: String,
    pub allowed_keys: Vec<String>,
}

impl AuthConfig {
    /// Get header value with case-insensitive matching
    /// HTTP headers are case-insensitive per RFC 7230
    pub fn get_header_value<'a>(&self, headers: &'a axum::http::HeaderMap) -> Option<&'a str> {
        // Try exact match first (most common case)
        if let Some(v) = headers
            .get(&self.required_header)
            .and_then(|h| h.to_str().ok())
        {
            return Some(v);
        }
        // Fallback to case-insensitive search
        let required_lower = self.required_header.to_lowercase();
        for (name, value) in headers.iter() {
            if name.as_str().to_lowercase() == required_lower {
                return value.to_str().ok();
            }
        }
        None
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerTlsConfig {
    pub enabled: bool,
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClusterTlsConfig {
    pub enabled: bool,
    pub ca_cert_path: String,
    pub client_cert_path: String,
    pub client_key_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Cookbook {
    pub models: Vec<Model>,
}

impl Cookbook {
    pub fn validate(&self) -> anyhow::Result<()> {
        for model in &self.models {
            for profile in &model.profiles {
                profile.validate(&model.name)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Model {
    pub name: String,
    pub description: Option<String>,
    #[serde(default = "default_model_enabled")]
    pub enabled: bool,
    pub profiles: Vec<Profile>,
}

fn default_model_enabled() -> bool {
    true
}

fn default_idle_timeout_seconds() -> u64 {
    300 // 5 minutes
}

fn default_body_read_timeout_ms() -> u64 {
    30_000 // 30 seconds
}

fn default_protocol_detect_timeout_ms() -> u64 {
    10_000 // 10 seconds
}

#[derive(Debug, Deserialize, Clone)]
pub struct Profile {
    pub id: String,
    pub description: Option<String>,
    /// Path to a local model file. Either `model_path` or `hf_repo` must be specified.
    #[serde(default)]
    pub model_path: Option<String>,
    /// Hugging Face repository (e.g., "ggml-org/Qwen2.5-0.5B-Instruct-GGUF").
    /// Use this instead of `model_path` to download models from Hugging Face.
    #[serde(default)]
    pub hf_repo: Option<String>,
    /// Hugging Face model file within the repository (e.g., "qwen2.5-0.5b-instruct-q4_k_m.gguf").
    /// Optional when using `hf_repo`; if omitted, llama-server will use the default file.
    #[serde(default)]
    pub hf_file: Option<String>,
    /// Idle timeout before instance is stopped. Default: 300 (5 minutes)
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    #[serde(default)]
    pub max_instances: Option<usize>,
    /// llama-server arguments as a shell-style string (e.g., "-fa on --no-kv-offload")
    #[serde(deserialize_with = "deserialize_llama_server_args")]
    pub llama_server_args: Vec<String>,
    #[serde(default)]
    pub max_wait_in_queue_ms: Option<u64>,
    #[serde(default)]
    pub max_request_duration_ms: Option<u64>,
    #[serde(default)]
    pub startup_timeout_seconds: Option<u64>,
    /// Timeout in seconds for downloading models from Hugging Face.
    /// Only used when `hf_repo` is specified. Default is 3600 (1 hour).
    /// This timeout covers both the download and the model loading time.
    #[serde(default)]
    pub download_timeout_seconds: Option<u64>,
    /// Override the global max_queue_size_per_model for this profile.
    #[serde(default)]
    pub max_queue_size: Option<usize>,
}

impl Profile {
    pub fn effective_max_instances(&self, defaults: &ModelDefaults) -> usize {
        match self.max_instances {
            Some(val) => val,
            None => defaults.max_instances_per_model.max(1),
        }
    }

    pub fn effective_max_queue_size(&self, defaults: &ModelDefaults) -> usize {
        self.max_queue_size.unwrap_or(defaults.max_queue_size_per_model)
    }

    /// Returns the effective startup timeout in seconds.
    /// When using `hf_repo` (Hugging Face download), uses `download_timeout_seconds` (default 3600s).
    /// Otherwise, uses `startup_timeout_seconds` (default 60s).
    pub fn effective_startup_timeout_seconds(&self) -> u64 {
        let uses_hf = self.hf_repo.is_some() || self.has_flag(&["-hfr", "--hf-repo"]);

        if uses_hf {
            // For HF downloads, use download_timeout_seconds (default 1 hour)
            self.download_timeout_seconds.unwrap_or(3600)
        } else {
            // For local models, use startup_timeout_seconds (default 60s)
            self.startup_timeout_seconds.unwrap_or(60)
        }
    }

    pub fn supports_embeddings(&self) -> bool {
        self.has_flag(&["--embedding", "--embeddings"])
    }

    pub fn supports_rerank(&self) -> bool {
        self.has_flag(&["--rerank", "--reranking"])
    }

    pub fn supports_text_mode(&self) -> bool {
        !self.supports_embeddings() && !self.supports_rerank()
    }

    pub fn validate(&self, model_name: &str) -> anyhow::Result<()> {
        // Validate model source: either model_path or hf_repo must be specified
        let has_model_path = self.model_path.is_some();
        let has_hf_repo = self.hf_repo.is_some();
        let has_model_in_args = self.has_flag(&["-m", "--model"]);
        let has_hf_in_args = self.has_flag(&["-hfr", "--hf-repo"]);

        if !has_model_path && !has_hf_repo && !has_model_in_args && !has_hf_in_args {
            return Err(anyhow::anyhow!(
                "Profile {}:{} must specify either 'model_path', 'hf_repo', or include -m/--model/-hfr/--hf-repo in llama_server_args",
                model_name, self.id
            ));
        }

        if has_model_path && has_hf_repo {
            return Err(anyhow::anyhow!(
                "Profile {}:{} specifies both 'model_path' and 'hf_repo'; use only one model source",
                model_name, self.id
            ));
        }

        // Warn if hf_file is specified without hf_repo
        if self.hf_file.is_some() && !has_hf_repo && !has_hf_in_args {
            tracing::warn!(
                "Profile {}:{} specifies 'hf_file' without 'hf_repo'; 'hf_file' will be ignored",
                model_name,
                self.id
            );
        }

        Ok(())
    }

    pub fn get_api_key(&self) -> Option<String> {
        let args = &self.llama_server_args;
        args.iter()
            .position(|arg| arg == "--api-key")
            .and_then(|idx| args.get(idx + 1).cloned())
            .or_else(|| {
                args.iter()
                    .find_map(|arg| arg.strip_prefix("--api-key=").map(|s| s.to_string()))
            })
    }

    fn has_flag(&self, needles: &[&str]) -> bool {
        let mut iter = self.llama_server_args.iter();
        iter.any(|arg| {
            let normalized = arg.trim().to_ascii_lowercase();
            needles.iter().any(|needle| {
                normalized == *needle || normalized.starts_with(&format!("{}=", needle))
            })
        })
    }
}

/// Load node configuration from a YAML file with environment variable overrides.
///
/// Environment variables with the `LLAMESH_` prefix override values from the config file.
/// Use `__` (double underscore) as the nesting separator for nested fields.
///
/// Examples:
/// - `LLAMESH_NODE_ID` → `node_id`
/// - `LLAMESH_LISTEN_ADDR` → `listen_addr`
/// - `LLAMESH_MAX_VRAM_MB` → `max_vram_mb`
/// - `LLAMESH_CLUSTER__ENABLED` → `cluster.enabled`
pub fn load_config(path: &Path) -> anyhow::Result<NodeConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::from(path))
        .add_source(
            config::Environment::with_prefix("LLAMESH")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()?;

    settings
        .try_deserialize()
        .context("Failed to parse config file")
}

pub fn load_cookbook(path: &Path) -> anyhow::Result<Cookbook> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read cookbook file: {}", path.display()))?;

    serde_yaml::from_str(&contents).context("Failed to parse cookbook file")
}

pub fn load_secrets(path: &Path) -> anyhow::Result<SecretsConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::from(path))
        .build()?;

    settings
        .try_deserialize()
        .context("Failed to parse secrets file")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ModelDefaults {
        ModelDefaults {
            max_concurrent_requests_per_instance: 4,
            max_queue_size_per_model: 32,
            max_instances_per_model: 5,
            max_wait_in_queue_ms: 60_000,
            max_request_duration_ms: 300_000,
        }
    }

    fn profile_with_args(args: &[&str]) -> Profile {
        Profile {
            id: "p".into(),
            description: None,
            model_path: Some("/tmp/model.gguf".into()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: args.iter().map(|s| s.to_string()).collect(),
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        }
    }

    #[test]
    fn effective_max_instances_falls_back_to_defaults() {
        let profile = profile_with_args(&[]);
        assert_eq!(profile.effective_max_instances(&defaults()), 5);
    }

    #[test]
    fn effective_max_instances_prefers_profile_value() {
        let mut profile = profile_with_args(&[]);
        profile.max_instances = Some(2);
        assert_eq!(profile.effective_max_instances(&defaults()), 2);
    }

    #[test]
    fn effective_max_instances_supports_zero() {
        let mut profile = profile_with_args(&[]);
        profile.max_instances = Some(0);
        assert_eq!(profile.effective_max_instances(&defaults()), 0);
    }

    #[test]
    fn effective_max_queue_size_falls_back_to_defaults() {
        let profile = profile_with_args(&[]);
        assert_eq!(profile.effective_max_queue_size(&defaults()), 32);
    }

    #[test]
    fn effective_max_queue_size_prefers_profile_value() {
        let mut profile = profile_with_args(&[]);
        profile.max_queue_size = Some(100);
        assert_eq!(profile.effective_max_queue_size(&defaults()), 100);
    }

    #[test]
    fn effective_max_queue_size_supports_zero() {
        let mut profile = profile_with_args(&[]);
        profile.max_queue_size = Some(0);
        assert_eq!(profile.effective_max_queue_size(&defaults()), 0);
    }

    #[test]
    fn detects_embedding_support() {
        let profile = profile_with_args(&["--embedding"]);
        assert!(profile.supports_embeddings());
        assert!(!profile.supports_text_mode());
    }

    #[test]
    fn detects_rerank_support() {
        let profile = profile_with_args(&["--reranking"]);
        assert!(profile.supports_rerank());
        assert!(!profile.supports_text_mode());
    }

    #[test]
    fn generic_profiles_support_text_mode() {
        let profile = profile_with_args(&["-c", "4096"]);
        assert!(profile.supports_text_mode());
        assert!(!profile.supports_embeddings());
        assert!(!profile.supports_rerank());
    }

    #[test]
    fn effective_startup_timeout_defaults_to_60_for_local_models() {
        let profile = profile_with_args(&["-c", "4096"]);
        assert_eq!(profile.effective_startup_timeout_seconds(), 60);
    }

    #[test]
    fn effective_startup_timeout_uses_custom_startup_timeout() {
        let mut profile = profile_with_args(&["-c", "4096"]);
        profile.startup_timeout_seconds = Some(120);
        assert_eq!(profile.effective_startup_timeout_seconds(), 120);
    }

    #[test]
    fn effective_startup_timeout_defaults_to_3600_for_hf_models() {
        let mut profile = profile_with_args(&["-c", "4096"]);
        profile.model_path = None;
        profile.hf_repo = Some("org/model-GGUF".into());
        assert_eq!(profile.effective_startup_timeout_seconds(), 3600);
    }

    #[test]
    fn effective_startup_timeout_uses_custom_download_timeout() {
        let mut profile = profile_with_args(&["-c", "4096"]);
        profile.model_path = None;
        profile.hf_repo = Some("org/model-GGUF".into());
        profile.download_timeout_seconds = Some(7200);
        assert_eq!(profile.effective_startup_timeout_seconds(), 7200);
    }

    #[test]
    fn effective_startup_timeout_detects_hf_in_args() {
        let profile = profile_with_args(&["--hf-repo", "org/model-GGUF"]);
        assert_eq!(profile.effective_startup_timeout_seconds(), 3600);
    }

    #[test]
    fn validate_cluster_tls_requires_server_tls() {
        let mut config = NodeConfig {
            node_id: "test".into(),
            listen_addr: "0.0.0.0:8080".into(),
            public_url: None,
            max_vram_mb: 0,
            max_sysmem_mb: 0,
            max_instances_per_node: 1,
            metrics_path: ".".into(),
            default_model: "m".into(),
            model_defaults: defaults(),
            llama_cpp_ports: None,
            llama_cpp: LlamaCppConfig {
                repo_url: "".into(),
                repo_path: "".into(),
                build_path: "".into(),
                binary_path: "".into(),
                branch: "".into(),
                build_args: vec![],
                build_command_args: vec![],
                auto_update_interval_seconds: 0,
                enabled: false,
                keep_builds: 3,
            },
            cluster: ClusterConfig {
                enabled: false,
                peers: vec![],
                gossip_interval_seconds: 1,
                max_concurrent_gossip: 16,
                discovery: Default::default(),
                noise: Default::default(),
                circuit_breaker: Default::default(),
                version_mismatch_action: "warn".to_string(),
            },
            http: HttpConfig {
                request_body_limit_bytes: 0,
                idle_timeout_seconds: 0,
                body_read_timeout_ms: 30_000,
                protocol_detect_timeout_ms: 10_000,
            },
            auth: None,
            server_tls: Some(ServerTlsConfig {
                enabled: false,
                cert_path: "".into(),
                key_path: "".into(),
            }),
            cluster_tls: Some(ClusterTlsConfig {
                enabled: true,
                ca_cert_path: "".into(),
                client_cert_path: "".into(),
                client_key_path: "".into(),
            }),
            shutdown_grace_period_seconds: 30,
            max_hops: 10,
            logging: None,
            max_total_queue_entries: 0,
        };

        assert!(config.validate().is_err());

        if let Some(ref mut server_tls) = config.server_tls {
            server_tls.enabled = true;
        }
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_profile_missing_model_source() {
        let profile = Profile {
            id: "test".into(),
            description: None,
            model_path: None,
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: vec!["-c".into(), "4096".into()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };
        assert!(profile.validate("test_model").is_err());
    }

    #[test]
    fn validate_profile_both_model_sources() {
        let profile = Profile {
            id: "test".into(),
            description: None,
            model_path: Some("/path/to/model.gguf".into()),
            hf_repo: Some("org/model-GGUF".into()),
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: vec![],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };
        assert!(profile.validate("test_model").is_err());
    }

    #[test]
    fn validate_profile_model_in_args() {
        let profile = Profile {
            id: "test".into(),
            description: None,
            model_path: None,
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: vec!["-m".into(), "/path/to/model.gguf".into()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };
        assert!(profile.validate("test_model").is_ok());
    }

    #[test]
    fn validate_profile_hf_in_args() {
        let profile = Profile {
            id: "test".into(),
            description: None,
            model_path: None,
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 10,
            max_instances: None,
            llama_server_args: vec!["--hf-repo".into(), "org/model-GGUF".into()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };
        assert!(profile.validate("test_model").is_ok());
    }

    #[test]
    fn supports_embedding_variant_flags() {
        // Test both --embedding and --embeddings variants
        let p1 = profile_with_args(&["--embedding"]);
        let p2 = profile_with_args(&["--embeddings"]);
        assert!(p1.supports_embeddings());
        assert!(p2.supports_embeddings());
    }

    #[test]
    fn supports_rerank_variant_flags() {
        // Test both --rerank and --reranking variants
        let p1 = profile_with_args(&["--rerank"]);
        let p2 = profile_with_args(&["--reranking"]);
        assert!(p1.supports_rerank());
        assert!(p2.supports_rerank());
    }

    #[test]
    fn test_has_flag() {
        let p = profile_with_args(&["-c", "4096", "--model", "/path/model.gguf"]);
        assert!(p.has_flag(&["-m", "--model"]));
        assert!(p.has_flag(&["-c", "--context-size"]));
        assert!(!p.has_flag(&["--embedding"]));
    }

    // Tests for llama_server_args string parsing
    mod llama_server_args_parsing {
        use super::*;

        fn parse_cookbook(yaml: &str) -> Result<Cookbook, serde_yaml::Error> {
            serde_yaml::from_str(yaml)
        }

        #[test]
        fn parses_simple_args() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "-fa on --no-kv-offload"
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert_eq!(args, &["-fa", "on", "--no-kv-offload"]);
        }

        #[test]
        fn parses_empty_string() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: ""
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert!(args.is_empty());
        }

        #[test]
        fn parses_quoted_args_single() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "--api-key 'my secret key'"
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert_eq!(args, &["--api-key", "my secret key"]);
        }

        #[test]
        fn parses_quoted_args_double() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: '-m "/path with spaces/model.gguf"'
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert_eq!(args, &["-m", "/path with spaces/model.gguf"]);
        }

        #[test]
        fn parses_json_in_args() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "--chat-template-kwargs '{\"reasoning_effort\": \"high\"}'"
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert_eq!(
                args,
                &["--chat-template-kwargs", "{\"reasoning_effort\": \"high\"}"]
            );
        }

        #[test]
        fn rejects_unmatched_quotes() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "--api-key 'unmatched"
"#;
            let result = parse_cookbook(yaml);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("unmatched") || err.contains("Invalid shell syntax"));
        }

        #[test]
        fn parses_key_equals_value() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "--ctx-size=4096 -fa=on"
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert_eq!(args, &["--ctx-size=4096", "-fa=on"]);
        }

        #[test]
        fn parses_whitespace_only_as_empty() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: "   "
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let args = &cookbook.models[0].profiles[0].llama_server_args;
            assert!(args.is_empty());
        }

        #[test]
        fn parses_max_queue_size() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        max_queue_size: 128
        llama_server_args: ""
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let profile = &cookbook.models[0].profiles[0];
            assert_eq!(profile.max_queue_size, Some(128));
        }

        #[test]
        fn max_queue_size_defaults_to_none() {
            let yaml = r#"
models:
  - name: test
    profiles:
      - id: default
        model_path: /tmp/model.gguf
        idle_timeout_seconds: 10
        llama_server_args: ""
"#;
            let cookbook = parse_cookbook(yaml).unwrap();
            let profile = &cookbook.models[0].profiles[0];
            assert_eq!(profile.max_queue_size, None);
        }
    }
}
