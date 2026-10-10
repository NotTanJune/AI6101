//! Option resolution of the `mcpe` launcher: sensible defaults so that `mcpe` alone starts the
//! coding agent in the current directory.
//!
//! [`resolve_mcpe`] is pure: it reads the environment, the home directory and the file system
//! only through the values and closures it is given, so tests can drive it without touching the
//! real ones.

use std::path::{Path, PathBuf};

use harness_agent::registry::ModelRegistry;

use crate::commands::{DEFAULT_TUI_BUDGET_USD, DEFAULT_TUI_MODEL, TuiOptions};

/// Models tried in order when no model is chosen: the first whose API key variable is set wins.
pub const AUTO_PICK_ORDER: [&str; 5] = ["luna", "flash", "sol", "pro", "astra"];

/// Environment variable that sets the default model.
pub const MODEL_ENV: &str = "MCPE_MODEL";

/// Workspace-relative MCP configuration candidates, in order.
pub const WORKSPACE_MCP_FILES: [&str; 2] = [".mcp.json", "mcp.json"];

/// User-level MCP configuration, relative to the home directory.
pub const HOME_MCP_FILE: &str = ".config/mcpe/mcp.json";

/// Workspace-relative Tool KB.
pub const WORKSPACE_KB_FILE: &str = ".mcpe/kb.json";

/// Workspace-relative session transcript directory.
pub const WORKSPACE_SESSIONS_DIR: &str = ".mcpe/sessions";

/// Environment variable that selects the colour theme when `--theme` is not given.
pub const THEME_ENV: &str = "MCPE_THEME";

/// User-level config file (theme and colour overrides), relative to the home directory.
pub const HOME_CONFIG_FILE: &str = ".config/mcpe/config.json";

/// Workspace-relative config file; it overrides the user-level one key by key.
pub const WORKSPACE_CONFIG_FILE: &str = ".mcpe/config.json";

/// What the user typed on the `mcpe` command line. Every field is optional.
#[derive(Debug, Clone, Default)]
pub struct McpeArgs {
    /// `-m/--model`.
    pub model: Option<String>,
    /// `-e/--effort`.
    pub effort: Option<String>,
    /// `-w/--workspace`.
    pub workspace: Option<PathBuf>,
    /// `--mcp-config`.
    pub mcp_config: Option<PathBuf>,
    /// `--kb`.
    pub kb: Option<PathBuf>,
    /// `--sessions-dir`.
    pub sessions_dir: Option<PathBuf>,
    /// `--budget-usd`.
    pub budget_usd: Option<f64>,
    /// `--models`: a registry file instead of the built-in one.
    pub models: Option<PathBuf>,
    /// `--mock-script`.
    pub mock_script: Option<PathBuf>,
    /// `--theme`: a built-in theme name.
    pub theme: Option<String>,
    /// `--config`: the only config file read, in place of discovery.
    pub config: Option<PathBuf>,
}

/// The world the resolution looks at.
pub struct Lookup<'a> {
    /// Environment variable lookup.
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a path is an existing file.
    pub is_file: &'a dyn Fn(&Path) -> bool,
    /// The current directory.
    pub cwd: &'a Path,
    /// The home directory, when known.
    pub home: Option<&'a Path>,
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// The message shown when no API key is set: it names every variable and the models it serves.
pub fn missing_key_hint(registry: &ModelRegistry) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for model in registry.models() {
        match groups.iter_mut().find(|(env, _)| *env == model.api_key_env) {
            Some((_, aliases)) => aliases.push(model.alias.clone()),
            None => groups.push((model.api_key_env.clone(), vec![model.alias.clone()])),
        }
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|(env, aliases)| format!("{env} ({})", aliases.join(", ")))
        .collect();
    let list = match parts.as_slice() {
        [] => "an API key".to_owned(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    };
    format!("no API key found: set {list}")
}

/// Resolve the `mcpe` command line into [`TuiOptions`].
///
/// - Model: `-m`, else `$MCPE_MODEL`, else the first of [`AUTO_PICK_ORDER`] (that the registry
///   has) whose API key variable is set, else `luna`, with a hint that names every key variable.
/// - Workspace: `-w`, else the current directory.
/// - MCP config: `--mcp-config`, else the first existing of `<workspace>/.mcp.json`,
///   `<workspace>/mcp.json`, `~/.config/mcpe/mcp.json`.
/// - Tool KB: `--kb`, else `<workspace>/.mcpe/kb.json` when it exists.
/// - Sessions: `--sessions-dir`, else `<workspace>/.mcpe/sessions`.
///
/// Explicit flags always win, even when the file they name does not exist (the session builder
/// then reports that file).
pub fn resolve_mcpe(args: McpeArgs, registry: &ModelRegistry, lookup: &Lookup<'_>) -> TuiOptions {
    let config_flag = args.config;
    let workspace = match args.workspace {
        Some(path) if path.is_absolute() => path,
        Some(path) => lookup.cwd.join(path),
        None => lookup.cwd.to_path_buf(),
    };

    let mut key_hint = None;
    let model = match args.model.or_else(|| non_blank((lookup.var)(MODEL_ENV))) {
        Some(model) => model,
        None => {
            let picked = AUTO_PICK_ORDER.iter().find(|alias| {
                registry
                    .get(alias)
                    .is_ok_and(|spec| non_blank((lookup.var)(&spec.api_key_env)).is_some())
            });
            match picked {
                Some(alias) => (*alias).to_owned(),
                None => {
                    key_hint = Some(missing_key_hint(registry));
                    DEFAULT_TUI_MODEL.to_owned()
                }
            }
        }
    };

    let mcp_config = args.mcp_config.or_else(|| {
        WORKSPACE_MCP_FILES
            .iter()
            .map(|name| workspace.join(name))
            .chain(lookup.home.map(|home| home.join(HOME_MCP_FILE)))
            .find(|path| (lookup.is_file)(path))
    });
    let kb = args
        .kb
        .or_else(|| Some(workspace.join(WORKSPACE_KB_FILE)).filter(|path| (lookup.is_file)(path)));
    let sessions_dir = args
        .sessions_dir
        .unwrap_or_else(|| workspace.join(WORKSPACE_SESSIONS_DIR));
    let config_sources = match config_flag {
        Some(_) => Vec::new(),
        None => lookup
            .home
            .map(|home| home.join(HOME_CONFIG_FILE))
            .into_iter()
            .chain(std::iter::once(workspace.join(WORKSPACE_CONFIG_FILE)))
            .filter(|path| (lookup.is_file)(path))
            .collect(),
    };

    TuiOptions {
        workspace: Some(workspace),
        model,
        effort: args.effort,
        models: args.models,
        mcp_config,
        kb,
        sessions_dir: Some(sessions_dir),
        budget_usd: args.budget_usd.unwrap_or(DEFAULT_TUI_BUDGET_USD),
        mock_script: args.mock_script,
        key_hint,
        theme: args.theme,
        config: config_flag,
        config_sources,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::commands::load_registry;

    struct World {
        vars: BTreeMap<String, String>,
        files: BTreeSet<PathBuf>,
        cwd: PathBuf,
        home: Option<PathBuf>,
    }

    impl World {
        fn new() -> Self {
            Self {
                vars: BTreeMap::new(),
                files: BTreeSet::new(),
                cwd: PathBuf::from("/work/app"),
                home: Some(PathBuf::from("/home/dev")),
            }
        }

        fn var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_owned(), value.to_owned());
            self
        }

        fn file(mut self, path: &str) -> Self {
            self.files.insert(PathBuf::from(path));
            self
        }

        fn resolve(&self, args: McpeArgs, registry: &ModelRegistry) -> TuiOptions {
            let var = |name: &str| self.vars.get(name).cloned();
            let is_file = |path: &Path| self.files.contains(path);
            resolve_mcpe(
                args,
                registry,
                &Lookup {
                    var: &var,
                    is_file: &is_file,
                    cwd: &self.cwd,
                    home: self.home.as_deref(),
                },
            )
        }
    }

    fn builtin() -> ModelRegistry {
        load_registry(None).expect("the built-in registry loads")
    }

    /// A registry with only `sol` (OpenAI) and `pro` (Gemini), to prove the order is applied.
    fn small() -> ModelRegistry {
        let text = crate::commands::EMBEDDED_MODELS_TOML;
        let mut registry = builtin();
        let keep: Vec<_> = registry
            .models()
            .iter()
            .filter(|m| m.alias == "sol" || m.alias == "pro")
            .cloned()
            .collect();
        assert_eq!(keep.len(), 2, "{text}");
        registry = ModelRegistry::from_specs(keep).expect("registry");
        registry
    }

    #[test]
    fn the_embedded_registry_is_the_checked_in_one() {
        let registry = builtin();
        assert_eq!(registry.aliases(), ["luna", "sol", "astra", "flash", "pro"]);
    }

    #[test]
    fn the_model_is_picked_by_which_api_key_is_set() {
        let registry = builtin();
        let openai = World::new().var("OPENAI_API_KEY", "dummy");
        assert_eq!(openai.resolve(McpeArgs::default(), &registry).model, "luna");
        let gemini = World::new().var("GEMINI_API_KEY", "dummy");
        assert_eq!(
            gemini.resolve(McpeArgs::default(), &registry).model,
            "flash"
        );
        let both = World::new()
            .var("GEMINI_API_KEY", "dummy")
            .var("OPENAI_API_KEY", "dummy");
        assert_eq!(both.resolve(McpeArgs::default(), &registry).model, "luna");
        let blank = World::new()
            .var("OPENAI_API_KEY", "  ")
            .var("GEMINI_API_KEY", "dummy");
        assert_eq!(
            blank.resolve(McpeArgs::default(), &registry).model,
            "flash",
            "a blank key is no key"
        );
        assert!(
            openai
                .resolve(McpeArgs::default(), &registry)
                .key_hint
                .is_none()
        );
    }

    #[test]
    fn the_auto_pick_order_skips_models_the_registry_lacks() {
        let registry = small();
        let both = World::new()
            .var("OPENAI_API_KEY", "dummy")
            .var("GEMINI_API_KEY", "dummy");
        assert_eq!(both.resolve(McpeArgs::default(), &registry).model, "sol");
        let gemini = World::new().var("GEMINI_API_KEY", "dummy");
        assert_eq!(gemini.resolve(McpeArgs::default(), &registry).model, "pro");
    }

    #[test]
    fn no_key_falls_back_to_luna_with_a_hint_naming_every_variable() {
        let options = World::new().resolve(McpeArgs::default(), &builtin());
        assert_eq!(options.model, "luna");
        assert_eq!(
            options.key_hint.as_deref(),
            Some(
                "no API key found: set OPENAI_API_KEY (luna, sol, astra) or GEMINI_API_KEY (flash, pro)"
            )
        );
    }

    #[test]
    fn flag_beats_environment_beats_auto_pick() {
        let registry = builtin();
        let world = World::new()
            .var("OPENAI_API_KEY", "dummy")
            .var("MCPE_MODEL", "sol");
        assert_eq!(world.resolve(McpeArgs::default(), &registry).model, "sol");
        let flagged = world.resolve(
            McpeArgs {
                model: Some("pro".to_owned()),
                ..McpeArgs::default()
            },
            &registry,
        );
        assert_eq!(flagged.model, "pro");
        assert!(flagged.key_hint.is_none());
        let blank = World::new()
            .var("MCPE_MODEL", " ")
            .var("GEMINI_API_KEY", "k");
        assert_eq!(blank.resolve(McpeArgs::default(), &registry).model, "flash");
    }

    #[test]
    fn the_workspace_defaults_to_the_current_directory() {
        let world = World::new();
        let options = world.resolve(McpeArgs::default(), &builtin());
        assert_eq!(options.workspace, Some(PathBuf::from("/work/app")));
        let explicit = world.resolve(
            McpeArgs {
                workspace: Some(PathBuf::from("/srv/other")),
                ..McpeArgs::default()
            },
            &builtin(),
        );
        assert_eq!(explicit.workspace, Some(PathBuf::from("/srv/other")));
        let relative = world.resolve(
            McpeArgs {
                workspace: Some(PathBuf::from("sub/dir")),
                ..McpeArgs::default()
            },
            &builtin(),
        );
        assert_eq!(relative.workspace, Some(PathBuf::from("/work/app/sub/dir")));
    }

    #[test]
    fn mcp_config_is_discovered_in_a_fixed_order() {
        let registry = builtin();
        let all = World::new()
            .file("/work/app/.mcp.json")
            .file("/work/app/mcp.json")
            .file("/home/dev/.config/mcpe/mcp.json");
        assert_eq!(
            all.resolve(McpeArgs::default(), &registry).mcp_config,
            Some(PathBuf::from("/work/app/.mcp.json"))
        );
        let no_dot = World::new()
            .file("/work/app/mcp.json")
            .file("/home/dev/.config/mcpe/mcp.json");
        assert_eq!(
            no_dot.resolve(McpeArgs::default(), &registry).mcp_config,
            Some(PathBuf::from("/work/app/mcp.json"))
        );
        let home_only = World::new().file("/home/dev/.config/mcpe/mcp.json");
        assert_eq!(
            home_only.resolve(McpeArgs::default(), &registry).mcp_config,
            Some(PathBuf::from("/home/dev/.config/mcpe/mcp.json"))
        );
        let none = World::new();
        assert_eq!(
            none.resolve(McpeArgs::default(), &registry).mcp_config,
            None
        );
        let mut no_home = World::new().file("/home/dev/.config/mcpe/mcp.json");
        no_home.home = None;
        assert_eq!(
            no_home.resolve(McpeArgs::default(), &registry).mcp_config,
            None
        );
    }

    #[test]
    fn discovery_follows_the_workspace_flag() {
        let world = World::new()
            .file("/work/app/.mcp.json")
            .file("/srv/other/mcp.json")
            .file("/srv/other/.mcpe/kb.json");
        let options = world.resolve(
            McpeArgs {
                workspace: Some(PathBuf::from("/srv/other")),
                ..McpeArgs::default()
            },
            &builtin(),
        );
        assert_eq!(
            options.mcp_config,
            Some(PathBuf::from("/srv/other/mcp.json"))
        );
        assert_eq!(options.kb, Some(PathBuf::from("/srv/other/.mcpe/kb.json")));
        assert_eq!(
            options.sessions_dir,
            Some(PathBuf::from("/srv/other/.mcpe/sessions"))
        );
    }

    #[test]
    fn explicit_paths_win_even_when_discovery_would_find_something_else() {
        let world = World::new()
            .file("/work/app/.mcp.json")
            .file("/work/app/.mcpe/kb.json");
        let options = world.resolve(
            McpeArgs {
                mcp_config: Some(PathBuf::from("./servers.json")),
                kb: Some(PathBuf::from("/kb/other.json")),
                sessions_dir: Some(PathBuf::from("/var/sessions")),
                models: Some(PathBuf::from("/etc/models.toml")),
                mock_script: Some(PathBuf::from("/mock")),
                effort: Some("high".to_owned()),
                budget_usd: Some(1.5),
                ..McpeArgs::default()
            },
            &builtin(),
        );
        assert_eq!(options.mcp_config, Some(PathBuf::from("./servers.json")));
        assert_eq!(options.kb, Some(PathBuf::from("/kb/other.json")));
        assert_eq!(options.sessions_dir, Some(PathBuf::from("/var/sessions")));
        assert_eq!(options.models, Some(PathBuf::from("/etc/models.toml")));
        assert_eq!(options.mock_script, Some(PathBuf::from("/mock")));
        assert_eq!(options.effort.as_deref(), Some("high"));
        assert_eq!(options.budget_usd, 1.5);
    }

    #[test]
    fn defaults_are_sensible_without_any_flags() {
        let options = World::new().resolve(McpeArgs::default(), &builtin());
        assert_eq!(options.kb, None);
        assert_eq!(options.mcp_config, None);
        assert_eq!(options.models, None, "the built-in registry");
        assert_eq!(options.budget_usd, 5.0);
        assert_eq!(options.effort, None);
        assert_eq!(
            options.sessions_dir,
            Some(PathBuf::from("/work/app/.mcpe/sessions"))
        );
    }

    #[test]
    fn config_files_are_discovered_global_then_workspace_or_replaced_by_config_flag() {
        let registry = builtin();
        let global = "/home/dev/.config/mcpe/config.json";
        let workspace = "/work/app/.mcpe/config.json";

        let only_global = World::new()
            .file(global)
            .resolve(McpeArgs::default(), &registry);
        assert_eq!(only_global.config_sources, vec![PathBuf::from(global)]);

        let both = World::new().file(global).file(workspace);
        let options = both.resolve(McpeArgs::default(), &registry);
        assert_eq!(
            options.config_sources,
            vec![PathBuf::from(global), PathBuf::from(workspace)]
        );

        let explicit = both.resolve(
            McpeArgs {
                config: Some(PathBuf::from("/x/c.json")),
                ..McpeArgs::default()
            },
            &registry,
        );
        assert_eq!(explicit.config, Some(PathBuf::from("/x/c.json")));
        assert!(explicit.config_sources.is_empty());
    }

    #[test]
    fn the_theme_flag_is_passed_to_the_session_options() {
        let registry = builtin();
        let options = World::new().resolve(
            McpeArgs {
                theme: Some("nord".to_owned()),
                ..McpeArgs::default()
            },
            &registry,
        );
        assert_eq!(options.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn the_hint_handles_one_and_three_key_variables() {
        let registry = small();
        assert_eq!(
            missing_key_hint(&registry),
            "no API key found: set OPENAI_API_KEY (sol) or GEMINI_API_KEY (pro)"
        );
        let one = ModelRegistry::from_specs(
            builtin()
                .models()
                .iter()
                .filter(|m| m.alias == "luna")
                .cloned()
                .collect(),
        )
        .expect("registry");
        assert_eq!(
            missing_key_hint(&one),
            "no API key found: set OPENAI_API_KEY (luna)"
        );
    }
}
