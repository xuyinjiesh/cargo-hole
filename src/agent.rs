pub mod codex;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

/// The config file, looked for in the crate root being worked on -- the
/// directory `--path` names, not the shell's working directory.
pub const CONFIG_FILE: &str = ".cargo-hole.toml";

/// The agent CLI to drive when neither the flag nor the config says otherwise.
pub const DEFAULT_CLI: &str = "codex";

/// Attempts per hole before giving up, when nothing overrides it.
///
/// Each attempt is a model call, so this is deliberately small: a model that
/// got it wrong three times will not be fixed by seven more identical prompts.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_MAX_TOKENS: u32 = 2048;

/// Environment variables, the layer between the config file and the flags.
pub const ENV_CLI: &str = "CARGO_HOLE_CLI";
pub const ENV_MODEL: &str = "CARGO_HOLE_MODEL";
pub const ENV_BASE_URL: &str = "CARGO_HOLE_BASE_URL";
pub const ENV_API_KEY: &str = "CARGO_HOLE_API_KEY";
pub const ENV_MAX_ATTEMPTS: &str = "CARGO_HOLE_MAX_ATTEMPTS";
pub const ENV_MAX_TOKENS: &str = "CARGO_HOLE_MAX_TOKENS";
pub const ENV_TEMPERATURE: &str = "CARGO_HOLE_TEMPERATURE";

pub enum Agent {
    Cli(Cli, ModelConfig),
    BuiltIn(ModelConfig),
}

pub enum Cli {
    Codex,
}

impl Cli {
    /// The CLI named by `name`, if it is one we know how to drive.
    ///
    /// An unknown name is an error rather than a fallback: `--cli codxe` should
    /// complain, not quietly run a different agent.
    pub fn from_name(name: &str) -> Result<Cli> {
        match name.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Cli::Codex),
            other => anyhow::bail!("unknown cli `{other}`; known: codex"),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Cli::Codex => "codex",
        }
    }
}

pub struct ModelConfig {
    pub base_url: String,
    pub model_name: String,
    pub api_key: String,
    /// Attempts per hole before giving up.
    pub max_attempts: u32,
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
    /// Sampling temperature. `0.0` keeps runs reproducible.
    pub temperature: f32,
}

/// The `[model]` section of `.cargo-hole.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSection {
    /// Model name. Empty or absent defers to the agent CLI's own config.
    model: Option<String>,
    /// Base URL, for providers that need one.
    base_url: Option<String>,
    /// Literal key. Prefer `api_key_env`, so the file stays safe to commit.
    api_key: Option<String>,
    /// Name of the environment variable holding the key.
    api_key_env: Option<String>,
}

/// The `[fill]` section of `.cargo-hole.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FillSection {
    max_attempts: Option<u32>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

/// The whole config file.
///
/// `deny_unknown_fields` turns a misspelled key into an error. Silently ignoring
/// `[modle]` would leave the user staring at defaults that look like the file
/// was never read.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    model: Option<ModelSection>,
    fill: Option<FillSection>,
}

impl ModelConfig {
    /// The `CARGO_HOLE_*` variables, with no config file involved.
    ///
    /// Split out from [`ModelConfig::load`] so the environment layer can be
    /// used -- and reasoned about -- on its own.
    ///
    /// An empty `model_name` means "whatever the CLI is already configured to
    /// use", which is the right default: an agent CLI has its own config naming
    /// a model, and silently overriding it would be surprising.
    pub fn from_env() -> ModelConfig {
        let mut config = ModelConfig::defaults();
        config.apply_env();
        config
    }

    /// The settings a run should use, given the crate being worked on.
    ///
    /// Three layers, most specific last: the built-in defaults, then
    /// `<root>/.cargo-hole.toml` if there is one, then the `CARGO_HOLE_*`
    /// variables. Command line flags are applied on top by
    /// [`ModelConfig::apply_overrides`].
    ///
    /// The config is looked up under `root` rather than under the working
    /// directory: `cargo hole fill --path elsewhere` must read `elsewhere`'s
    /// config, not the shell's.
    pub fn load(root: &Path) -> ModelConfig {
        let mut config = ModelConfig::from_config(&root.join(CONFIG_FILE));
        config.apply_env();
        config
    }

    /// Settings read from a `.cargo-hole.toml`.
    ///
    /// A missing file is not an error -- every key has a default, so a repo
    /// with no config still runs. Only the keys that are present are honoured,
    /// so a file may name just the model and nothing else.
    ///
    /// The environment is deliberately *not* consulted here: this reports what
    /// the file says, and [`ModelConfig::from_env`] layers `CARGO_HOLE_*` on
    /// top of it.
    pub fn from_config(path: &Path) -> ModelConfig {
        let mut config = ModelConfig::defaults();

        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<FileConfig>(&text) {
                Ok(file) => {
                    if let Some(section) = &file.model {
                        config.apply_model(section);
                    }
                    if let Some(section) = &file.fill {
                        config.apply_fill(section);
                    }
                }
                // Reported rather than fatal: a broken config should not make
                // `list` -- which needs no model at all -- refuse to run.
                Err(e) => eprintln!("warning: ignoring {}: {e}", path.display()),
            },
            // `NotFound` is the common case, not a problem. Any other read
            // error (permissions, a directory) is worth mentioning.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("warning: cannot read {}: {e}", path.display()),
        }

        config
    }

    fn defaults() -> ModelConfig {
        ModelConfig {
            base_url: String::new(),
            model_name: String::new(),
            api_key: String::new(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: 0.0,
        }
    }

    fn apply_model(&mut self, section: &ModelSection) {
        if let Some(model) = &section.model {
            self.model_name = model.clone();
        }
        if let Some(base_url) = &section.base_url {
            self.base_url = base_url.clone();
        }
        // A literal key is honoured, but `api_key_env` wins: the file is meant
        // to be committed, so naming the variable is the safer spelling, and
        // having both means the environment copy is the newer one.
        if let Some(key) = &section.api_key {
            self.api_key = key.clone();
        }
        if let Some(var) = &section.api_key_env {
            match std::env::var(var) {
                Ok(key) if !key.trim().is_empty() => self.api_key = key,
                _ => eprintln!("warning: ignoring api_key_env: `{var}` is not set (or is empty)"),
            }
        }
    }

    fn apply_fill(&mut self, section: &FillSection) {
        if let Some(attempts) = section.max_attempts {
            self.max_attempts = attempts;
        }
        if let Some(tokens) = section.max_tokens {
            self.max_tokens = tokens;
        }
        if let Some(temperature) = section.temperature {
            self.temperature = temperature;
        }
    }

    /// Environment overrides, read as `CARGO_HOLE_<FIELD>`.
    ///
    /// These win over the config file, which is what makes
    /// `CARGO_HOLE_MODEL=... cargo hole fill` work without editing anything.
    /// An empty value is ignored rather than applied: `CARGO_HOLE_MODEL=` in a
    /// shell profile should not silently erase a model named in the config.
    ///
    /// A typo here cannot be reported the way a config key can -- the
    /// environment is not ours to validate -- but a value that fails to parse
    /// is called out rather than dropped.
    fn apply_env(&mut self) {
        if let Some(value) = env_non_empty(ENV_MODEL) {
            self.model_name = value;
        }
        if let Some(value) = env_non_empty(ENV_BASE_URL) {
            self.base_url = value;
        }
        if let Some(value) = env_non_empty(ENV_API_KEY) {
            self.api_key = value;
        }
        if let Some(value) = env_non_empty(ENV_MAX_ATTEMPTS) {
            match value.parse() {
                Ok(attempts) => self.max_attempts = attempts,
                Err(e) => eprintln!("warning: ignoring {ENV_MAX_ATTEMPTS}: {e}"),
            }
        }
        if let Some(value) = env_non_empty(ENV_MAX_TOKENS) {
            match value.parse() {
                Ok(tokens) => self.max_tokens = tokens,
                Err(e) => eprintln!("warning: ignoring {ENV_MAX_TOKENS}: {e}"),
            }
        }
        if let Some(value) = env_non_empty(ENV_TEMPERATURE) {
            match value.parse() {
                Ok(temperature) => self.temperature = temperature,
                Err(e) => eprintln!("warning: ignoring {ENV_TEMPERATURE}: {e}"),
            }
        }
    }

    /// Apply the command line on top of everything else.
    ///
    /// The flags win over both the config file and the environment, and an
    /// absent flag leaves the value that was already resolved in place -- so
    /// `--model ""` still defers, exactly as [`Agent::with_model`] does.
    pub fn apply_overrides(
        &mut self,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) {
        if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
            self.model_name = model.to_string();
        }
        if let Some(base_url) = base_url.filter(|u| !u.trim().is_empty()) {
            self.base_url = base_url.to_string();
        }
        if let Some(api_key) = api_key.filter(|k| !k.trim().is_empty()) {
            self.api_key = api_key.to_string();
        }
    }
}

/// A `CARGO_HOLE_*` variable, treating unset and empty alike.
fn env_non_empty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

impl Agent {
    /// The agent named on the command line, configured from the environment and
    /// the config file.
    ///
    /// Precedence, most specific last: defaults, `.cargo-hole.toml`, the
    /// `CARGO_HOLE_*` variables, then these flags. `cli` empty means "whatever
    /// the environment names", falling back to [`DEFAULT_CLI`].
    pub fn from_args(
        cli: Option<&str>,
        root: &Path,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<Agent> {
        let mut config = ModelConfig::load(root);
        config.apply_overrides(model, base_url, api_key);

        let cli = cli
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| env_non_empty(ENV_CLI))
            .unwrap_or_else(|| DEFAULT_CLI.to_string());

        Ok(Agent::Cli(Cli::from_name(&cli)?, config))
    }

    /// The named CLI with its configuration resolved.
    pub fn cli(cli: impl Into<String>, root: &Path) -> Result<Agent> {
        Agent::from_args(Some(&cli.into()), root, None, None, None)
    }

    /// A one-line description, for the `filling N hole(s) ... using ...` header.
    ///
    /// An empty model is reported as the CLI's own default rather than as a
    /// blank, since "which model ran" is the first thing to check when output
    /// looks wrong.
    pub fn label(&self) -> String {
        match self {
            Agent::Cli(cli, config) => {
                if config.model_name.trim().is_empty() {
                    return format!("cli:{} (its own model)", cli.name());
                } else {
                    return format!("cli:{}:{}", cli.name(), config.model_name);
                }
            }
            Agent::BuiltIn(config) => {
                if config.model_name.trim().is_empty() {
                    "builtin (default model)".to_string()
                } else {
                    return format!("builtin:{}", config.model_name);
                }
            }
        }
    }

    /// Override the model name, leaving everything else alone.
    ///
    /// An empty name is ignored so that `--model ""` (or no flag at all) keeps
    /// deferring to the CLI's own configuration.
    pub fn with_model(mut self, model_name: impl Into<String>) -> Agent {
        let model_name = model_name.into();
        if model_name.trim().is_empty() {
            return self;
        }
        match &mut self {
            Agent::Cli(_, config) | Agent::BuiltIn(config) => config.model_name = model_name,
        }
        self
    }

    pub fn handle(&self, prompt: &str) -> Result<String> {
        match self {
            Agent::Cli(cli, model) => cli.handle(model, prompt),
            // Not implemented yet. An error rather than `todo!()`: panicking
            // inside a library is never the right failure mode, and this path is
            // reachable from the CLI.
            Agent::BuiltIn(_) => anyhow::bail!(
                "the built-in model agent is not implemented; use `--agent cli --cli codex` \
                 or configure an external agent"
            ),
        }
    }

    /// Attempts per hole, as resolved from the config, the environment and the
    /// flags. The filler owns the retry loop, so it asks rather than assumes.
    pub fn max_attempts(&self) -> u32 {
        match self {
            Agent::Cli(_, config) | Agent::BuiltIn(config) => config.max_attempts,
        }
    }
}

/// The agent to reach for when none is configured: the built-in one.
///
/// It has no config file to read -- `BuiltIn` is not implemented yet, and
/// [`Agent::from_args`], the path every command actually takes, is the one that
/// reads `<root>/.cargo-hole.toml`.
impl Default for Agent {
    fn default() -> Agent {
        Agent::BuiltIn(ModelConfig::from_env())
    }
}

impl Cli {
    pub fn handle(&self, model: &ModelConfig, prompt: &str) -> Result<String> {
        match self {
            Cli::Codex => codex::handle(model, prompt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory unique per call, so parallel tests cannot collide.
    fn temp_root() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "cargo-hole-agent-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the temp root");
        root
    }

    /// A root holding `text` as its `.cargo-hole.toml`.
    fn root_with(text: &str) -> PathBuf {
        let root = temp_root();
        std::fs::write(root.join(CONFIG_FILE), text).expect("write the temp config");
        root
    }

    #[test]
    fn without_a_config_file_the_defaults_stand() {
        let config = ModelConfig::load(&temp_root());
        assert!(
            config.model_name.is_empty(),
            "the CLI's own model is the default"
        );
        assert_eq!(config.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(config.temperature, 0.0);
    }

    #[test]
    fn the_config_is_read_from_the_crate_root_not_the_cwd() {
        // `--path` elsewhere must read elsewhere's config, whatever the shell
        // happens to be sitting in.
        let root = root_with("[model]\nmodel = \"qwen3.7-max\"\n");
        assert_eq!(ModelConfig::load(&root).model_name, "qwen3.7-max");
        assert!(ModelConfig::load(&temp_root()).model_name.is_empty());
    }

    #[test]
    fn the_file_is_read_section_by_section() {
        let config = ModelConfig::load(&root_with(
            r#"
            [model]
            model = "qwen3.7-max"
            base_url = "http://127.0.0.1:11434/v1"
            api_key = "sk-from-file"

            [fill]
            max_attempts = 7
            max_tokens = 4096
            temperature = 0.5
            "#,
        ));
        assert_eq!(config.model_name, "qwen3.7-max");
        assert_eq!(config.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(config.api_key, "sk-from-file");
        assert_eq!(config.max_attempts, 7);
        assert_eq!(config.max_tokens, 4096);
        assert_eq!(config.temperature, 0.5);
    }

    #[test]
    fn keys_the_file_omits_keep_their_defaults() {
        let config = ModelConfig::load(&root_with("[model]\nmodel = \"qwen3.7-max\"\n"));
        assert_eq!(config.model_name, "qwen3.7-max");
        assert!(config.base_url.is_empty());
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn a_misspelled_key_is_not_silently_ignored() {
        // The whole file is dropped rather than half-applied: a typo like
        // `[modle]` must not look like a working config.
        let config = ModelConfig::load(&root_with("[modle]\nmodel = \"qwen3.7-max\"\n"));
        assert!(config.model_name.is_empty());
    }

    #[test]
    fn flags_win_over_the_file() {
        let mut config = ModelConfig::load(&root_with("[model]\nmodel = \"from-file\"\n"));
        config.apply_overrides(Some("from-flag"), None, None);
        assert_eq!(config.model_name, "from-flag");
    }

    #[test]
    fn an_empty_flag_does_not_erase_the_file() {
        let mut config = ModelConfig::load(&root_with("[model]\nmodel = \"from-file\"\n"));
        config.apply_overrides(Some(""), None, None);
        assert_eq!(config.model_name, "from-file");
    }

    #[test]
    fn an_unknown_cli_is_an_error_not_a_fallback() {
        assert!(Cli::from_name("codex").is_ok());
        assert!(Cli::from_name("Codex").is_ok());
        assert!(Cli::from_name("codxe").is_err());
    }

    #[test]
    fn the_label_distinguishes_a_named_model_from_the_default() {
        let agent = Agent::from_args(Some("codex"), &temp_root(), None, None, None)
            .expect("codex is known");
        assert_eq!(agent.label(), "cli:codex (its own model)");
        assert_eq!(
            agent.with_model("qwen3.7-max").label(),
            "cli:codex:qwen3.7-max"
        );
    }

    #[test]
    fn a_config_chosen_model_reaches_the_agent() {
        let root = root_with("[model]\nmodel = \"qwen3.7-max\"\n\n[fill]\nmax_attempts = 9\n");
        let agent = Agent::from_args(None, &root, None, None, None).expect("codex is the fallback");
        assert_eq!(agent.label(), "cli:codex:qwen3.7-max");
        assert_eq!(agent.max_attempts(), 9, "the config sets the retry budget");
    }
}
