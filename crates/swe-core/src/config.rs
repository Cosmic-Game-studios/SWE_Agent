//! Configuration, credentials and the provider registry.
//!
//! Everything lives under one home directory (`~/.swe-agent`, or
//! `$SWE_AGENT_HOME`):
//!
//! ```text
//! config.toml        settings and custom providers
//! credentials.json   API keys entered in the UI (mode 0600)
//! sessions/<id>/     one directory per session
//! worktrees/         git worktrees of isolated sub-agents
//! ```
//!
//! API keys are looked up in the environment first and in `credentials.json`
//! second, so a key exported in the shell always wins and nothing has to be
//! written to disk at all.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use swe_llm::{LlmError, ModelInfo, Protocol, Provider, ProviderSettings};
use swe_protocol::{Effort, PermissionMode};

use crate::mcp::McpServerConfig;

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    /// `$SWE_AGENT_HOME`, or `~/.swe-agent`.
    pub fn discover() -> Self {
        let home = std::env::var_os("SWE_AGENT_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".swe-agent")))
            .unwrap_or_else(|| PathBuf::from(".swe-agent"));
        Paths { home }
    }

    pub fn at(home: impl Into<PathBuf>) -> Self {
        Paths { home: home.into() }
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.home.join("credentials.json")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.home.join("worktrees")
    }
}

/// A provider defined in `config.toml`, or an override of a built-in one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub base_url: String,
    /// Display name; defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Set for local servers that need no key at all.
    #[serde(default)]
    pub no_key: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_mode: PermissionMode,
    pub default_effort: Effort,
    /// Provider and model for sub-agents; they use the parent's when unset.
    pub subagent_provider: Option<String>,
    pub subagent_model: Option<String>,
    /// Summarise the conversation once the context in use exceeds this many
    /// tokens (or 80% of the model's window, whichever is smaller).
    pub compact_threshold_tokens: u64,
    /// Upper bound on model requests per turn; a guard against runaway loops.
    pub max_steps_per_turn: u32,
    /// How many sub-agents deep delegation may go.
    pub max_agent_depth: u8,
    /// Ask the agent to verify once when it wants to finish a turn in which it
    /// changed files without running anything afterwards.
    pub verify_before_finish: bool,
    /// After a step that edited files, run the project's check (chosen from the
    /// project type) where the permission mode allows commands unasked.
    pub auto_check: bool,
    /// The check to run instead of the automatically chosen one, for example
    /// `make lint`.
    pub check_command: Option<String>,
    /// Keep the tools of MCP servers out of the model's context until the model
    /// switches a server on with the `mcp_tools` tool.
    pub dynamic_mcp_tools: bool,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_provider: None,
            default_model: None,
            default_mode: PermissionMode::default(),
            default_effort: Effort::default(),
            subagent_provider: None,
            subagent_model: None,
            compact_threshold_tokens: 200_000,
            max_steps_per_turn: 1_000,
            max_agent_depth: 2,
            verify_before_finish: true,
            auto_check: true,
            check_command: None,
            dynamic_mcp_tools: true,
            providers: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> anyhow::Result<Self> {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).map_err(|err| anyhow::anyhow!("{}: {err}", file.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(anyhow::anyhow!("{}: {err}", file.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        std::fs::create_dir_all(&paths.home)?;
        write_atomic(&paths.config_file(), toml::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

/// Write via a temporary file and rename, so a crash never leaves a half file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

struct Builtin {
    id: &'static str,
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
    key_envs: &'static [&'static str],
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "anthropic",
        name: "Anthropic",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        key_envs: &["ANTHROPIC_API_KEY"],
    },
    Builtin {
        id: "openai",
        name: "OpenAI",
        protocol: Protocol::OpenaiResponses,
        base_url: "https://api.openai.com/v1",
        key_envs: &["OPENAI_API_KEY"],
    },
    Builtin {
        id: "google",
        name: "Google Gemini",
        protocol: Protocol::Gemini,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        key_envs: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    },
    Builtin {
        id: "openrouter",
        name: "OpenRouter",
        protocol: Protocol::OpenaiChat,
        base_url: "https://openrouter.ai/api/v1",
        key_envs: &["OPENROUTER_API_KEY"],
    },
];

/// Id of the offline demo provider (enabled with `SWE_AGENT_DEMO=1`).
pub const DEMO_PROVIDER: &str = "demo";

/// Where a provider's API key comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum KeyStatus {
    /// Found in this environment variable.
    Env {
        variable: String,
    },
    /// Stored in `credentials.json`.
    Stored,
    /// The provider needs no key.
    NotNeeded,
    Missing,
}

impl KeyStatus {
    pub fn is_usable(&self) -> bool {
        !matches!(self, KeyStatus::Missing)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub key: KeyStatus,
    /// Environment variables checked for the key.
    pub key_envs: Vec<String>,
    /// False for providers that come from `config.toml`.
    pub builtin: bool,
}

struct Definition {
    name: String,
    protocol: Protocol,
    base_url: String,
    key_envs: Vec<String>,
    no_key: bool,
    headers: Vec<(String, String)>,
    builtin: bool,
}

struct CachedModels {
    fetched: Instant,
    models: Vec<ModelInfo>,
}

const MODEL_CACHE_TTL: Duration = Duration::from_secs(600);

/// Knows every configured provider, resolves credentials and hands out
/// ready-to-use adapters.
pub struct ProviderRegistry {
    paths: Paths,
    definitions: RwLock<BTreeMap<String, Definition>>,
    stored_keys: RwLock<BTreeMap<String, String>>,
    instances: Mutex<HashMap<String, Arc<dyn Provider>>>,
    /// Adapters injected by tests or embedding code; never rebuilt.
    injected: RwLock<HashMap<String, Arc<dyn Provider>>>,
    models: Mutex<HashMap<String, CachedModels>>,
}

impl ProviderRegistry {
    pub fn new(paths: &Paths, config: &Config) -> Self {
        let registry = ProviderRegistry {
            paths: paths.clone(),
            definitions: RwLock::new(BTreeMap::new()),
            stored_keys: RwLock::new(load_credentials(paths)),
            instances: Mutex::new(HashMap::new()),
            injected: RwLock::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
        };
        registry.apply_config(config);
        registry
    }

    /// (Re)build the provider table from the built-ins plus `config.providers`.
    pub fn apply_config(&self, config: &Config) {
        let mut definitions = BTreeMap::new();
        for builtin in BUILTINS {
            definitions.insert(
                builtin.id.to_string(),
                Definition {
                    name: builtin.name.to_string(),
                    protocol: builtin.protocol,
                    base_url: builtin.base_url.to_string(),
                    key_envs: builtin.key_envs.iter().map(|s| s.to_string()).collect(),
                    no_key: false,
                    headers: Vec::new(),
                    builtin: true,
                },
            );
        }
        if std::env::var("SWE_AGENT_DEMO").is_ok_and(|v| v == "1" || v == "true") {
            definitions.insert(
                DEMO_PROVIDER.to_string(),
                Definition {
                    name: "Demo (offline)".to_string(),
                    protocol: Protocol::Mock,
                    base_url: String::new(),
                    key_envs: Vec::new(),
                    no_key: true,
                    headers: Vec::new(),
                    builtin: true,
                },
            );
        }
        for (id, custom) in &config.providers {
            let builtin = definitions.get(id);
            let mut key_envs: Vec<String> = custom.api_key_env.iter().cloned().collect();
            if let Some(builtin) = builtin {
                key_envs.extend(builtin.key_envs.iter().cloned());
            }
            definitions.insert(
                id.clone(),
                Definition {
                    name: custom
                        .name
                        .clone()
                        .or_else(|| builtin.map(|b| b.name.clone()))
                        .unwrap_or_else(|| id.clone()),
                    protocol: custom.protocol,
                    base_url: custom.base_url.trim_end_matches('/').to_string(),
                    key_envs,
                    no_key: custom.no_key,
                    headers: custom.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    builtin: builtin.is_some(),
                },
            );
        }
        *self.definitions.write().unwrap() = definitions;
        self.instances.lock().unwrap().clear();
    }

    /// Register a ready-made adapter under `id` (tests, embedding).
    pub fn inject(&self, id: &str, provider: Arc<dyn Provider>) {
        self.injected.write().unwrap().insert(id.to_string(), provider);
    }

    fn resolve_key(&self, id: &str, definition: &Definition) -> (Option<String>, KeyStatus) {
        if definition.no_key {
            return (None, KeyStatus::NotNeeded);
        }
        for variable in &definition.key_envs {
            if let Ok(value) = std::env::var(variable)
                && !value.trim().is_empty()
            {
                return (
                    Some(value.trim().to_string()),
                    KeyStatus::Env {
                        variable: variable.clone(),
                    },
                );
            }
        }
        match self.stored_keys.read().unwrap().get(id) {
            Some(key) => (Some(key.clone()), KeyStatus::Stored),
            None => (None, KeyStatus::Missing),
        }
    }

    pub fn list(&self) -> Vec<ProviderInfo> {
        let definitions = self.definitions.read().unwrap();
        let mut providers: Vec<ProviderInfo> = definitions
            .iter()
            .map(|(id, definition)| ProviderInfo {
                id: id.clone(),
                name: definition.name.clone(),
                protocol: definition.protocol,
                base_url: definition.base_url.clone(),
                key: self.resolve_key(id, definition).1,
                key_envs: definition.key_envs.clone(),
                builtin: definition.builtin,
            })
            .collect();
        for (id, provider) in self.injected.read().unwrap().iter() {
            if !definitions.contains_key(id) {
                providers.push(ProviderInfo {
                    id: id.clone(),
                    name: id.clone(),
                    protocol: provider.protocol(),
                    base_url: String::new(),
                    key: KeyStatus::NotNeeded,
                    key_envs: Vec::new(),
                    builtin: false,
                });
            }
        }
        providers
    }

    /// The adapter for `id`, built on first use.
    pub fn get(&self, id: &str) -> Result<Arc<dyn Provider>, LlmError> {
        if let Some(provider) = self.injected.read().unwrap().get(id) {
            return Ok(provider.clone());
        }
        if let Some(provider) = self.instances.lock().unwrap().get(id) {
            return Ok(provider.clone());
        }
        let settings = {
            let definitions = self.definitions.read().unwrap();
            let definition = definitions
                .get(id)
                .ok_or_else(|| LlmError::Config(format!("unknown provider `{id}`")))?;
            let (api_key, status) = self.resolve_key(id, definition);
            if status == KeyStatus::Missing {
                let hint = match definition.key_envs.first() {
                    Some(variable) => format!("add it in Settings or set {variable}"),
                    None => "add it in Settings".to_string(),
                };
                return Err(LlmError::Auth(format!("no API key for {} ({hint})", definition.name)));
            }
            ProviderSettings {
                id: id.to_string(),
                protocol: definition.protocol,
                base_url: definition.base_url.clone(),
                api_key,
                headers: definition.headers.clone(),
            }
        };
        let provider = swe_llm::build_provider(settings)?;
        self.instances.lock().unwrap().insert(id.to_string(), provider.clone());
        Ok(provider)
    }

    /// Store (or with `None`, forget) the API key for a provider.
    pub fn set_key(&self, id: &str, key: Option<&str>) -> anyhow::Result<()> {
        if !self.definitions.read().unwrap().contains_key(id) {
            anyhow::bail!("unknown provider `{id}`");
        }
        {
            let mut keys = self.stored_keys.write().unwrap();
            match key.map(str::trim).filter(|k| !k.is_empty()) {
                Some(key) => keys.insert(id.to_string(), key.to_string()),
                None => keys.remove(id),
            };
            save_credentials(&self.paths, &keys)?;
        }
        self.instances.lock().unwrap().remove(id);
        self.models.lock().unwrap().remove(id);
        Ok(())
    }

    /// Models of a provider, cached for a few minutes.
    pub async fn models(&self, id: &str, refresh: bool) -> Result<Vec<ModelInfo>, LlmError> {
        if !refresh
            && let Some(cached) = self.models.lock().unwrap().get(id)
            && cached.fetched.elapsed() < MODEL_CACHE_TTL
        {
            return Ok(cached.models.clone());
        }
        let models = self.get(id)?.list_models().await?;
        self.models.lock().unwrap().insert(
            id.to_string(),
            CachedModels {
                fetched: Instant::now(),
                models: models.clone(),
            },
        );
        Ok(models)
    }

    /// Limits of a model if its provider's listing has been fetched already.
    pub fn cached_model(&self, provider: &str, model: &str) -> Option<ModelInfo> {
        let models = self.models.lock().unwrap();
        models
            .get(provider)?
            .models
            .iter()
            .find(|info| info.id == model)
            .cloned()
    }
}

fn load_credentials(paths: &Paths) -> BTreeMap<String, String> {
    std::fs::read_to_string(paths.credentials_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_credentials(paths: &Paths, keys: &BTreeMap<String, String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.home)?;
    let file = paths.credentials_file();
    // Create with restrictive permissions before any secret is written.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = file.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        {
            use std::io::Write;
            let mut handle = options.open(&tmp)?;
            handle.write_all(serde_json::to_string_pretty(keys)?.as_bytes())?;
        }
        std::fs::rename(&tmp, &file)?;
    }
    #[cfg(not(unix))]
    write_atomic(&file, serde_json::to_string_pretty(keys)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults_apply() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        assert_eq!(Config::load(&paths).unwrap(), Config::default());

        std::fs::write(
            paths.config_file(),
            r#"
default_provider = "ollama"
default_model = "qwen3-coder"
default_mode = "auto_edit"

[providers.ollama]
protocol = "openai-chat"
base_url = "http://localhost:11434/v1/"
no_key = true
"#,
        )
        .unwrap();
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.default_mode, PermissionMode::AutoEdit);
        assert_eq!(
            config.compact_threshold_tokens, 200_000,
            "unset fields keep their defaults"
        );
        config.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), config);

        let registry = ProviderRegistry::new(&paths, &config);
        let ollama = registry.list().into_iter().find(|p| p.id == "ollama").unwrap();
        assert_eq!(ollama.key, KeyStatus::NotNeeded);
        assert_eq!(ollama.base_url, "http://localhost:11434/v1");
        assert!(!ollama.builtin);
        assert!(registry.get("ollama").is_ok());
    }

    #[test]
    fn keys_are_stored_privately_and_drive_availability() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let registry = ProviderRegistry::new(&paths, &Config::default());
        let ids: Vec<String> = registry.list().into_iter().map(|p| p.id).collect();
        assert!(
            ["anthropic", "google", "openai", "openrouter"]
                .iter()
                .all(|id| ids.contains(&id.to_string()))
        );

        // Not asserting on env-provided keys: the test machine may have some.
        let before = registry.list().into_iter().find(|p| p.id == "openrouter").unwrap().key;
        if before == KeyStatus::Missing {
            assert!(matches!(registry.get("openrouter"), Err(LlmError::Auth(_))));
        }
        registry.set_key("openrouter", Some("  sk-or-test  ")).unwrap();
        if before == KeyStatus::Missing {
            assert_eq!(
                registry.list().into_iter().find(|p| p.id == "openrouter").unwrap().key,
                KeyStatus::Stored
            );
        }
        assert!(registry.get("openrouter").is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.credentials_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A fresh registry reads the stored key back.
        let reloaded = ProviderRegistry::new(&paths, &Config::default());
        assert!(reloaded.get("openrouter").is_ok());
        reloaded.set_key("openrouter", None).unwrap();
        assert!(
            !std::fs::read_to_string(paths.credentials_file())
                .unwrap()
                .contains("sk-or-test")
        );
        assert!(registry.set_key("nope", Some("x")).is_err());
    }
}
