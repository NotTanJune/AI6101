//! Slash command parsing. Pure: no state, no I/O.
//!
//! A message is a command when its first word is `/` followed by letters only (`/model`,
//! `/mcp`). Anything else, including paths such as `/etc/hosts is missing`, is sent to the model
//! as a normal message.

/// Subcommands of `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    /// List servers.
    List,
    /// Connect a configured server.
    Connect(String),
    /// Disconnect a server.
    Disconnect(String),
}

/// Subcommands of `/kb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KbCommand {
    /// Show the KB summary.
    Show,
    /// Snapshot an unmanaged server's live tools into the session KB.
    Adopt(String),
}

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/model` (picker) or `/model <alias>`.
    Model(Option<String>),
    /// `/effort <level>`.
    Effort(String),
    /// `/mcp ...`.
    Mcp(McpCommand),
    /// `/kb ...`.
    Kb(KbCommand),
    /// `/drift`.
    Drift,
    /// `/cost`.
    Cost,
    /// `/clear`.
    Clear,
    /// `/help`.
    Help,
    /// `/theme` (picker) or `/theme <name>`.
    Theme(Option<String>),
    /// `/quit`.
    Quit,
}

/// Usage text per command, shown with argument errors.
const USAGE_MODEL: &str = "usage: /model [alias]";
const USAGE_EFFORT: &str = "usage: /effort <level>";
const USAGE_MCP: &str = "usage: /mcp | /mcp connect <name> | /mcp disconnect <name>";
const USAGE_KB: &str = "usage: /kb | /kb adopt <server>";
const USAGE_THEME: &str = "usage: /theme [name]";

/// Parse a submitted line.
///
/// Returns `None` when the line is a normal message, `Some(Ok(..))` for a command and
/// `Some(Err(message))` for a command with bad arguments or an unknown name.
pub fn parse(line: &str) -> Option<Result<SlashCommand, String>> {
    let trimmed = line.trim();
    let first = trimmed.split_whitespace().next()?;
    let name = first.strip_prefix('/')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let args: Vec<&str> = trimmed.split_whitespace().skip(1).collect();
    Some(match (name, args.as_slice()) {
        ("model", []) => Ok(SlashCommand::Model(None)),
        ("model", [alias]) => Ok(SlashCommand::Model(Some((*alias).to_owned()))),
        ("model", _) => Err(USAGE_MODEL.to_owned()),
        ("effort", [level]) => Ok(SlashCommand::Effort((*level).to_owned())),
        ("effort", _) => Err(USAGE_EFFORT.to_owned()),
        ("mcp", []) => Ok(SlashCommand::Mcp(McpCommand::List)),
        ("mcp", ["connect", name]) => {
            Ok(SlashCommand::Mcp(McpCommand::Connect((*name).to_owned())))
        }
        ("mcp", ["disconnect", name]) => Ok(SlashCommand::Mcp(McpCommand::Disconnect(
            (*name).to_owned(),
        ))),
        ("mcp", _) => Err(USAGE_MCP.to_owned()),
        ("kb", []) => Ok(SlashCommand::Kb(KbCommand::Show)),
        ("kb", ["adopt", server]) => Ok(SlashCommand::Kb(KbCommand::Adopt((*server).to_owned()))),
        ("kb", _) => Err(USAGE_KB.to_owned()),
        ("drift", []) => Ok(SlashCommand::Drift),
        ("cost", []) => Ok(SlashCommand::Cost),
        ("clear", []) => Ok(SlashCommand::Clear),
        ("help", []) => Ok(SlashCommand::Help),
        ("theme", []) => Ok(SlashCommand::Theme(None)),
        ("theme", [name]) => Ok(SlashCommand::Theme(Some((*name).to_owned()))),
        ("theme", _) => Err(USAGE_THEME.to_owned()),
        ("quit" | "exit", []) => Ok(SlashCommand::Quit),
        ("drift" | "cost" | "clear" | "help" | "quit" | "exit", _) => {
            Err(format!("/{name} takes no arguments"))
        }
        _ => Err(format!("unknown command /{name}; type /help for the list")),
    })
}

/// Command list shown by `/help`: `(usage, description)`.
pub const COMMAND_HELP: &[(&str, &str)] = &[
    ("/model [alias]", "pick a model, or switch to one directly"),
    (
        "/effort <level>",
        "set the reasoning effort of the current model",
    ),
    ("/mcp", "list MCP servers and their status"),
    ("/mcp connect <name>", "connect a configured MCP server"),
    ("/mcp disconnect <name>", "disconnect an MCP server"),
    ("/kb", "show the session Tool KB"),
    (
        "/kb adopt <server>",
        "snapshot a server's live tools into the KB",
    ),
    ("/drift", "review drift between the KB and live servers"),
    ("/cost", "spent, unreconciled and remaining budget"),
    (
        "/theme [name]",
        "pick a colour theme (this session; set it permanently in config.json)",
    ),
    ("/clear", "clear the visible transcript"),
    ("/help", "this help"),
    ("/quit", "quit"),
];
