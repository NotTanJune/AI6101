//! `harness` command line entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use harness_cli::commands;

/// MCP tool-knowledge adaptation harness.
#[derive(Debug, Parser)]
#[command(name = "harness", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the interactive coding agent.
    #[command(long_about = "Open the interactive coding agent.\n\n\
        Chat with a model that can read and change files in the workspace and run shell \
        commands; writes and shell commands need your approval. Switch model and reasoning \
        effort with /model and /effort, manage MCP servers with /mcp, inspect the Tool KB with \
        /kb and handle drift between a server's live tools and the KB with /drift. Type /help \
        inside for the full list.\n\n\
        API keys are read from the environment variables named in the model registry \
        (OPENAI_API_KEY, GEMINI_API_KEY) and are never asked for, stored or shown. Every \
        session writes a redacted transcript.jsonl under --sessions-dir. With --mock-script \
        the model is replaced by scripted replies and nothing touches the network.")]
    Tui {
        /// Workspace the agent works in [default: the current directory].
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Starting model alias.
        #[arg(long, default_value = commands::DEFAULT_TUI_MODEL)]
        model: String,
        /// Starting reasoning effort [default: the model's default effort].
        #[arg(long)]
        effort: Option<String>,
        /// Model registry file [default: the registry built into the binary].
        #[arg(long)]
        models: Option<PathBuf>,
        /// MCP server configuration (standard mcp.json) [default: no servers].
        #[arg(long)]
        mcp_config: Option<PathBuf>,
        /// Tool KB file (JSON); servers with cards in it are managed [default: no KB].
        #[arg(long)]
        kb: Option<PathBuf>,
        /// Directory for session transcripts [default: <workspace>/.harness/sessions].
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        /// Budget cap for the session in USD.
        #[arg(long, default_value_t = commands::DEFAULT_TUI_BUDGET_USD)]
        budget_usd: f64,
        /// Directory of scripted model replies (01.json, 02.json, ...); no network.
        #[arg(long)]
        mock_script: Option<PathBuf>,
        /// Built-in colour theme, e.g. gruvbox-dark-hard [default: $MCPE_THEME, else tokyo-night].
        #[arg(long)]
        theme: Option<String>,
    },
    /// Validate a configuration and its inputs without contacting providers.
    #[command(
        long_about = "Validate a configuration and its inputs without contacting providers.\n\n\
        Loads the configuration, opens the input adapter, checks that the tasks, versions, \
        strategies, provider kind, runner kind, budget and rate card are usable, and prints \
        the server and tool counts, the strategy set and the schema versions. No provider, \
        network or live MCP server is contacted.\n\n\
        Exit codes:\n  0  valid\n  1  invalid (every problem is listed on stderr)"
    )]
    Validate {
        /// Experiment configuration (TOML).
        #[arg(long)]
        config: PathBuf,
    },
    /// Run the configured strategy/version/task/repetition matrix headlessly.
    #[command(
        long_about = "Run the configured strategy/version/task/repetition matrix headlessly.\n\n\
        Writes manifest.toml, events.jsonl, summary.json, kb/ and python/ under --output and \
        prints a per-strategy summary. Cost figures cover priced calls only: mock usage is not \
        USD, unknown usage is never shown as zero, and the dispatch cap is a local guard, not \
        the provider's bill.\n\n\
        Exit codes:\n  0  run finished ok\n  1  setup, configuration or I/O error (nothing was run)\n  \
        2  command line usage error\n  3  run finished with status failed\n  \
        4  run was cancelled\n  5  run stopped because the local dispatch budget was exhausted"
    )]
    Run {
        /// Experiment configuration (TOML).
        #[arg(long)]
        config: PathBuf,
        /// Output directory for the run artifacts.
        #[arg(long)]
        output: PathBuf,
        /// Replace the artifacts of a previous run in --output instead of refusing.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let code = match cli.command {
        Command::Tui {
            workspace,
            model,
            effort,
            models,
            mcp_config,
            kb,
            sessions_dir,
            budget_usd,
            mock_script,
            theme,
        } => commands::tui(
            &commands::TuiOptions {
                workspace,
                model,
                effort,
                models,
                mcp_config,
                kb,
                sessions_dir,
                budget_usd,
                mock_script,
                key_hint: None,
                theme,
                ..commands::TuiOptions::default()
            },
            &mut err,
        ),
        Command::Validate { config } => commands::validate(&config, &mut out, &mut err),
        Command::Run {
            config,
            output,
            force,
        } => commands::run(&config, &output, force, &mut out, &mut err),
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
