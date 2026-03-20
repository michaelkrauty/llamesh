//! Model index for O(1) model lookups.

use crate::config::{Cookbook, Profile};
use crate::instance::compute_args_hash;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Build index for O(1) model lookups.
/// Key: lowercase "model:profile", Value: (canonical_name, Profile)
pub fn build_model_index(cookbook: &Cookbook) -> HashMap<String, (String, Profile)> {
    let mut index = HashMap::new();
    for model in &cookbook.models {
        if !model.enabled {
            continue;
        }
        for profile in &model.profiles {
            let key = format!(
                "{}:{}",
                model.name.to_lowercase(),
                profile.id.to_lowercase()
            );
            index.insert(key, (model.name.clone(), profile.clone()));
        }
    }
    index
}

/// Model index manager for resolving model requests.
pub struct ModelIndex {
    index: Arc<RwLock<HashMap<String, (String, Profile)>>>,
}

impl ModelIndex {
    /// Create a new model index from a cookbook.
    pub fn new(cookbook: &Cookbook) -> Self {
        Self {
            index: Arc::new(RwLock::new(build_model_index(cookbook))),
        }
    }

    /// Rebuild the index from a new cookbook.
    pub async fn rebuild(&self, cookbook: &Cookbook) {
        let new_index = build_model_index(cookbook);
        let mut index = self.index.write().await;
        *index = new_index;
    }

    /// Resolve a model name to (canonical_name, Profile).
    /// Supports "model" (uses default profile) or "model:profile".
    pub async fn resolve(&self, model_name: &str) -> Option<(String, Profile)> {
        let parts: Vec<&str> = model_name.split(':').collect();
        let name = parts[0].to_lowercase();
        let profile_id = if parts.len() > 1 { parts[1] } else { "default" };
        let key = format!("{}:{}", name, profile_id.to_lowercase());

        let index = self.index.read().await;
        index.get(&key).cloned()
    }

    /// Get the raw index (for internal use).
    #[allow(dead_code)]
    pub fn get_raw(&self) -> Arc<RwLock<HashMap<String, (String, Profile)>>> {
        self.index.clone()
    }
}

/// Build the pre-args list (without --host/--port) for args_hash computation.
/// Returns (args, model_arg_present, hf_repo_arg_present).
pub fn build_pre_args(profile: &Profile) -> (Vec<String>, bool, bool) {
    let mut args = Vec::new();
    let mut model_arg_present = false;
    let mut hf_repo_arg_present = false;
    let mut ctx_size_present = false;
    let mut fit_present = false;

    for arg in &profile.llama_server_args {
        // Expand key=value to separate args for consistency
        if (arg.starts_with("--") || arg.starts_with("-")) && arg.contains('=') {
            if let Some((flag, val)) = arg.split_once('=') {
                args.push(flag.to_string());
                args.push(val.to_string());
            } else {
                args.push(arg.clone());
            }
        } else {
            args.push(arg.clone());
        }

        // Track model source flags
        if arg == "-m" || arg == "--model" || arg.starts_with("-m=") || arg.starts_with("--model=")
        {
            model_arg_present = true;
        }
        if arg == "-hfr"
            || arg == "--hf-repo"
            || arg.starts_with("-hfr=")
            || arg.starts_with("--hf-repo=")
        {
            hf_repo_arg_present = true;
        }
        if arg == "-c"
            || arg == "--ctx-size"
            || arg.starts_with("-c=")
            || arg.starts_with("--ctx-size=")
        {
            ctx_size_present = true;
        }
        if arg == "--fit" || arg.starts_with("--fit=") {
            fit_present = true;
        }
    }

    // Add model source if not in llama_server_args
    if !model_arg_present && !hf_repo_arg_present {
        if let Some(ref hf_repo) = profile.hf_repo {
            args.push("-hfr".to_string());
            args.push(hf_repo.clone());
            if let Some(ref hf_file) = profile.hf_file {
                args.push("-hff".to_string());
                args.push(hf_file.clone());
            }
        } else if let Some(ref model_path) = profile.model_path {
            args.push("-m".to_string());
            args.push(model_path.clone());
        }
    }

    // Add -c 0 (max context) if no context size specified
    if !ctx_size_present {
        args.push("-c".to_string());
        args.push("0".to_string());
    }

    // Add --fit off (disable auto-optimization) if not specified
    // This preserves our hand-crafted cookbook settings from being overridden
    if !fit_present {
        args.push("--fit".to_string());
        args.push("off".to_string());
    }

    (args, model_arg_present, hf_repo_arg_present)
}

/// Compute args_hash for a "model:profile" or "model" key.
/// Returns None if model/profile not found in cookbook.
pub async fn get_args_hash_for_key(cookbook: &RwLock<Cookbook>, key: &str) -> Option<String> {
    let (model_name, profile_id) = if let Some((m, p)) = key.split_once(':') {
        (m, p)
    } else {
        (key, "default")
    };

    let cookbook = cookbook.read().await;
    let model = cookbook.models.iter().find(|m| m.name == model_name)?;
    let profile = model.profiles.iter().find(|p| p.id == profile_id)?;

    let (pre_args, _, _) = build_pre_args(profile);
    Some(compute_args_hash(&pre_args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Model;

    fn sample_cookbook() -> Cookbook {
        Cookbook {
            models: vec![
                Model {
                    name: "gpt".to_string(),
                    description: None,
                    enabled: true,
                    profiles: vec![
                        Profile {
                            id: "default".to_string(),
                            description: None,
                            model_path: Some("/path/to/model.gguf".to_string()),
                            hf_repo: None,
                            hf_file: None,
                            idle_timeout_seconds: 300,
                            max_instances: Some(2),
                            llama_server_args: vec![],
                            max_wait_in_queue_ms: None,
                            max_request_duration_ms: None,
                            startup_timeout_seconds: None,
                            download_timeout_seconds: None,
                            max_queue_size: None,
                        },
                        Profile {
                            id: "fast".to_string(),
                            description: None,
                            model_path: Some("/path/to/model.gguf".to_string()),
                            hf_repo: None,
                            hf_file: None,
                            idle_timeout_seconds: 60,
                            max_instances: Some(4),
                            llama_server_args: vec![],
                            max_wait_in_queue_ms: None,
                            max_request_duration_ms: None,
                            startup_timeout_seconds: None,
                            download_timeout_seconds: None,
                            max_queue_size: None,
                        },
                    ],
                },
                Model {
                    name: "disabled".to_string(),
                    description: None,
                    enabled: false,
                    profiles: vec![],
                },
            ],
        }
    }

    #[test]
    fn test_build_model_index() {
        let cookbook = sample_cookbook();
        let index = build_model_index(&cookbook);

        assert!(index.contains_key("gpt:default"));
        assert!(index.contains_key("gpt:fast"));
        assert!(!index.contains_key("disabled:default"));
    }

    #[tokio::test]
    async fn test_resolve_model() {
        let cookbook = sample_cookbook();
        let model_index = ModelIndex::new(&cookbook);

        // Explicit profile
        let result = model_index.resolve("gpt:fast").await;
        assert!(result.is_some());
        let (name, profile) = result.unwrap();
        assert_eq!(name, "gpt");
        assert_eq!(profile.id, "fast");

        // Default profile
        let result = model_index.resolve("gpt").await;
        assert!(result.is_some());
        let (name, profile) = result.unwrap();
        assert_eq!(name, "gpt");
        assert_eq!(profile.id, "default");

        // Case insensitive
        let result = model_index.resolve("GPT:FAST").await;
        assert!(result.is_some());

        // Not found
        let result = model_index.resolve("nonexistent").await;
        assert!(result.is_none());
    }

    #[test]
    fn test_build_pre_args_with_model_path() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/path/to/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--ctx-size".to_string(), "2048".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, model_present, hf_present) = build_pre_args(&profile);

        assert!(!model_present);
        assert!(!hf_present);
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"/path/to/model.gguf".to_string()));
    }

    #[test]
    fn test_build_pre_args_with_hf_repo() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: None,
            hf_repo: Some("TheBloke/Llama-2-7B-GGUF".to_string()),
            hf_file: Some("llama-2-7b.Q4_K_M.gguf".to_string()),
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec![],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, model_present, hf_present) = build_pre_args(&profile);

        assert!(!model_present);
        assert!(!hf_present);
        assert!(args.contains(&"-hfr".to_string()));
        assert!(args.contains(&"TheBloke/Llama-2-7B-GGUF".to_string()));
        assert!(args.contains(&"-hff".to_string()));
    }

    #[tokio::test]
    async fn test_model_index_rebuild() {
        let cookbook = sample_cookbook();
        let model_index = ModelIndex::new(&cookbook);

        // Initially has gpt model
        assert!(model_index.resolve("gpt").await.is_some());

        // Rebuild with empty cookbook
        let empty_cookbook = Cookbook { models: vec![] };
        model_index.rebuild(&empty_cookbook).await;

        // Now gpt should not be found
        assert!(model_index.resolve("gpt").await.is_none());
    }

    #[test]
    fn test_build_pre_args_key_value_expansion() {
        // Test that --key=value args in llama_server_args are expanded to --key value
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--ctx-size=4096".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // --key=value should become --key value
        assert!(args.contains(&"--ctx-size".to_string()));
        assert!(args.contains(&"4096".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_model_flag_if_in_args() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/default.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["-m".to_string(), "/custom.gguf".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, model_present, _) = build_pre_args(&profile);
        // Should detect -m in args and not add model_path
        assert!(model_present);
        // The /default.gguf should NOT be added
        assert!(!args.contains(&"/default.gguf".to_string()));
    }

    #[test]
    fn test_build_pre_args_adds_ctx_zero_when_missing() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec![], // No context size specified
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should add -c 0 when no context size specified
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"0".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_ctx_zero_when_ctx_size_present() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--ctx-size".to_string(), "4096".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add -c when --ctx-size already present
        assert!(!args.contains(&"-c".to_string()));
        assert!(args.contains(&"--ctx-size".to_string()));
        assert!(args.contains(&"4096".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_ctx_zero_when_c_flag_present() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["-c".to_string(), "8192".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add extra -c when -c already present
        let c_count = args.iter().filter(|a| *a == "-c").count();
        assert_eq!(c_count, 1);
        assert!(args.contains(&"8192".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_ctx_zero_when_ctx_size_equals_format() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--ctx-size=2048".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add -c when --ctx-size=value already present
        assert!(!args.contains(&"-c".to_string()));
        assert!(args.contains(&"--ctx-size".to_string()));
        assert!(args.contains(&"2048".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_ctx_zero_when_c_equals_format() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["-c=4096".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add extra -c when -c=value already present
        let c_count = args.iter().filter(|a| *a == "-c").count();
        assert_eq!(c_count, 1);
        assert!(args.contains(&"4096".to_string()));
    }

    #[test]
    fn test_build_pre_args_adds_fit_off_when_missing() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec![], // No --fit specified
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should add --fit off when no --fit specified
        assert!(args.contains(&"--fit".to_string()));
        assert!(args.contains(&"off".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_fit_off_when_fit_present() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--fit".to_string(), "on".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add --fit off when --fit already present
        let fit_count = args.iter().filter(|a| *a == "--fit").count();
        assert_eq!(fit_count, 1);
        assert!(args.contains(&"on".to_string()));
        assert!(!args.contains(&"off".to_string()));
    }

    #[test]
    fn test_build_pre_args_skips_fit_off_when_fit_equals_format() {
        let profile = Profile {
            id: "test".to_string(),
            description: None,
            model_path: Some("/model.gguf".to_string()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 300,
            max_instances: Some(1),
            llama_server_args: vec!["--fit=on".to_string()],
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
        };

        let (args, _, _) = build_pre_args(&profile);
        // Should NOT add --fit off when --fit=value already present
        let fit_count = args.iter().filter(|a| *a == "--fit").count();
        assert_eq!(fit_count, 1);
        assert!(args.contains(&"on".to_string()));
        assert!(!args.contains(&"off".to_string()));
    }
}
