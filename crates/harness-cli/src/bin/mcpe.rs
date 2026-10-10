//! `mcpe`: the coding agent with sensible defaults. `mcpe` alone opens it in the current
//! directory; everything else is an optional flag.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use harness_cli::commands;
use harness_cli::launcher::McpeArgs;

#[derive(Debug, Parser)]
#[command(
    name = "mcpe",
    version,
    about = "Coding agent for your terminal, with MCP tool knowledge",
    after_help = "\
Examples:
  mcpe                              open the agent in the current directory
  mcpe -m flash -e high             start with the flash model at high effort
  mcpe --mcp-config ./servers.json  use these MCP servers
  mcpe --theme gruvbox-dark-hard    use a built-in colour theme

Defaults:
  model       $MCPE_MODEL, else the first of luna, flash, sol, pro, astra whose API key
              variable (OPENAI_API_KEY or GEMINI_API_KEY) is set
  theme       $MCPE_THEME, else \"theme\" in config.json, else tokyo-night
  config      ~/.config/mcpe/config.json, then <workspace>/.mcpe/config.json (workspace wins)
  workspace   the current directory
  MCP servers <workspace>/.mcp.json, <workspace>/mcp.json, ~/.config/mcpe/mcp.json (first found)
  Tool KB     <workspace>/.mcpe/kb.json (when it exists)
  sessions    <workspace>/.mcpe/sessions

Keys are read from the environment and never asked for, stored or shown. Type /help inside."
)]
struct Cli {
    /// Model alias [default: $MCPE_MODEL, else picked by which API key is set].
    #[arg(short, long)]
    model: Option<String>,
    /// Reasoning effort [default: the model's default effort].
    #[arg(short, long)]
    effort: Option<String>,
    /// Workspace the agent works in [default: the current directory].
    #[arg(short, long)]
    workspace: Option<PathBuf>,
    /// MCP server configuration (standard mcp.json) [default: discovered, see below].
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// Tool KB file (JSON) [default: <workspace>/.mcpe/kb.json when it exists].
    #[arg(long)]
    kb: Option<PathBuf>,
    /// Directory for session transcripts [default: <workspace>/.mcpe/sessions].
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
    /// Budget cap for the session in USD [default: 5].
    #[arg(long)]
    budget_usd: Option<f64>,
    /// Model registry file [default: the registry built into the binary].
    #[arg(long)]
    models: Option<PathBuf>,
    /// Directory of scripted model replies (01.json, 02.json, ...); no network.
    #[arg(long)]
    mock_script: Option<PathBuf>,
    /// Built-in colour theme, e.g. gruvbox-dark-hard [default: $MCPE_THEME, else config.json, else tokyo-night].
    #[arg(long)]
    theme: Option<String>,
    /// Config file (JSON) with theme and colour overrides; replaces the discovered config files.
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let args = McpeArgs {
        model: cli.model,
        effort: cli.effort,
        workspace: cli.workspace,
        mcp_config: cli.mcp_config,
        kb: cli.kb,
        sessions_dir: cli.sessions_dir,
        budget_usd: cli.budget_usd,
        models: cli.models,
        mock_script: cli.mock_script,
        theme: cli.theme,
        config: cli.config,
    };
    let code = commands::mcpe(args, &mut std::io::stderr());
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
