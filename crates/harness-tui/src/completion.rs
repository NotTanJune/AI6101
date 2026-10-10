//! Slash command completion. Pure: the matching logic takes the input text and a small
//! [`Context`] (models, current model, MCP servers) and returns suggestions; nothing here
//! touches the UI or the app state.
//!
//! Suggestions appear only while the input is a single line that starts with `/` and whose
//! first word is letters only (`/model`, never `/etc/hosts`). Command names come from
//! [`COMMAND_HELP`], the single source of command descriptions.

use std::collections::BTreeMap;

use crate::app::ModelInfo;
use crate::bridge::{McpServerStatus, McpState};
use crate::commands::{self, COMMAND_HELP, KbCommand, McpCommand, SlashCommand};
use crate::theme::builtin;
use crate::theme::palette::luminance;

/// Most suggestion rows shown at once; more scroll.
pub const MAX_ROWS: usize = 8;

/// What the matcher needs to know about the session.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// Models offered by `/model`.
    pub models: &'a [ModelInfo],
    /// The model in use (for `/effort`).
    pub current_model: Option<&'a ModelInfo>,
    /// Configured MCP servers.
    pub mcp: &'a BTreeMap<String, McpServerStatus>,
}

/// One suggestion row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Text that replaces the token being completed (`/model`, `luna`, `connect`).
    pub insert: String,
    /// Short description shown next to it.
    pub detail: String,
    /// Whether the completed word expects more arguments (a trailing space is inserted).
    pub more_args: bool,
}

/// The suggestions for the current input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Heading of the popup (`commands`, `models`, ...).
    pub title: &'static str,
    /// Byte offset in the input where the token being completed starts.
    pub token_start: usize,
    /// What has been typed of that token.
    pub prefix: String,
    /// Matching suggestions, best first.
    pub items: Vec<Suggestion>,
}

/// Suggestion state kept in the app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuggestState {
    /// Selected suggestion row.
    pub selected: usize,
    /// The input text for which the user pressed Esc; suggestions stay hidden until it changes.
    pub dismissed_for: Option<String>,
    /// The input text the selection belongs to; a different text resets the selection.
    pub last_text: String,
}

/// `(name without slash, description, takes more arguments)` for every command, in help order.
fn command_table() -> Vec<(&'static str, &'static str, bool)> {
    let mut table: Vec<(&'static str, &'static str, bool)> = Vec::new();
    for (usage, what) in COMMAND_HELP {
        let mut words = usage.split_whitespace();
        let Some(name) = words.next().and_then(|w| w.strip_prefix('/')) else {
            continue;
        };
        let has_args = words.next().is_some();
        match table.iter_mut().find(|(n, _, _)| *n == name) {
            Some(entry) => entry.2 |= has_args,
            None => table.push((name, what, has_args)),
        }
    }
    table
}

fn is_letters(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic())
}

fn matches_prefix(candidate: &str, prefix: &str) -> bool {
    candidate
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
}

fn server_detail(status: &McpServerStatus) -> String {
    match &status.state {
        McpState::Disconnected => "disconnected".to_owned(),
        McpState::Connecting => "connecting".to_owned(),
        McpState::Failed(_) => "failed".to_owned(),
        McpState::Connected if status.managed => format!("connected, {} tools", status.live_tools),
        McpState::Connected => format!("connected, {} tools, not in the KB", status.live_tools),
    }
}

fn server_items(ctx: &Context<'_>, prefix: &str) -> Vec<Suggestion> {
    ctx.mcp
        .values()
        .filter(|s| matches_prefix(&s.name, prefix))
        .map(|s| Suggestion {
            insert: s.name.clone(),
            detail: server_detail(s),
            more_args: false,
        })
        .collect()
}

fn word_items(words: &[(&str, &str)], prefix: &str, more_args: bool) -> Vec<Suggestion> {
    words
        .iter()
        .filter(|(word, _)| matches_prefix(word, prefix))
        .map(|(word, detail)| Suggestion {
            insert: (*word).to_owned(),
            detail: (*detail).to_owned(),
            more_args,
        })
        .collect()
}

/// Suggestions for `input`, or `None` when it is not a command being typed (or nothing
/// matches). The cursor is assumed to be at the end of the input.
pub fn suggest(input: &str, ctx: &Context<'_>) -> Option<Completion> {
    if input.contains('\n') || !input.starts_with('/') {
        return None;
    }
    let body = &input[1..];
    let table = command_table();
    let Some(split) = body.find(char::is_whitespace) else {
        // Still typing the command name.
        if !body.is_empty() && !is_letters(body) {
            return None;
        }
        let items: Vec<Suggestion> = table
            .iter()
            .filter(|(name, _, _)| matches_prefix(name, body))
            .map(|(name, what, more)| Suggestion {
                insert: format!("/{name}"),
                detail: (*what).to_owned(),
                more_args: *more,
            })
            .collect();
        return finish("commands", 0, input.to_owned(), items);
    };
    let command = &body[..split];
    if !is_letters(command) || !table.iter().any(|(name, _, _)| name == &command) {
        return None;
    }
    let rest = &body[split..];
    let args: Vec<&str> = rest.split_whitespace().collect();
    let trailing = rest.ends_with(char::is_whitespace);
    let (index, prefix) = if trailing {
        (args.len(), "")
    } else {
        (
            args.len().saturating_sub(1),
            args.last().copied().unwrap_or(""),
        )
    };
    let token_start = input.len() - prefix.len();
    let (title, items) = match (command, index) {
        ("model", 0) => (
            "models",
            ctx.models
                .iter()
                .filter(|m| matches_prefix(&m.alias, prefix))
                .map(|m| Suggestion {
                    insert: m.alias.clone(),
                    detail: format!(
                        "{} · efforts {}",
                        m.provider,
                        m.allowed_efforts
                            .iter()
                            .map(|e| e.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    more_args: false,
                })
                .collect::<Vec<_>>(),
        ),
        ("effort", 0) => (
            "effort levels",
            ctx.current_model
                .map(|m| {
                    m.allowed_efforts
                        .iter()
                        .filter(|e| matches_prefix(e.as_str(), prefix))
                        .map(|e| Suggestion {
                            insert: e.as_str().to_owned(),
                            detail: if *e == m.default_effort {
                                format!("{} default", m.alias)
                            } else {
                                format!("for {}", m.alias)
                            },
                            more_args: false,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        ),
        ("theme", 0) => (
            "themes",
            builtin::BUILTINS
                .iter()
                .filter(|b| matches_prefix(b.slug, prefix))
                .map(|b| Suggestion {
                    insert: b.slug.to_owned(),
                    detail: if luminance(builtin::theme(b).bg) < 0.5 {
                        "dark"
                    } else {
                        "light"
                    }
                    .to_owned(),
                    more_args: false,
                })
                .collect::<Vec<_>>(),
        ),
        ("mcp", 0) => (
            "mcp",
            word_items(
                &[
                    ("connect", "connect a configured MCP server"),
                    ("disconnect", "disconnect an MCP server"),
                ],
                prefix,
                true,
            ),
        ),
        ("mcp", 1) if matches!(args.first(), Some(&("connect" | "disconnect"))) => {
            ("servers", server_items(ctx, prefix))
        }
        ("kb", 0) => (
            "kb",
            word_items(
                &[("adopt", "snapshot a server's live tools into the KB")],
                prefix,
                true,
            ),
        ),
        ("kb", 1) if args.first() == Some(&"adopt") => ("servers", server_items(ctx, prefix)),
        _ => return None,
    };
    finish(title, token_start, prefix.to_owned(), items)
}

fn finish(
    title: &'static str,
    token_start: usize,
    prefix: String,
    items: Vec<Suggestion>,
) -> Option<Completion> {
    // A single suggestion that is already fully typed and takes nothing more has nothing to add.
    if let [only] = items.as_slice()
        && !only.more_args
        && only.insert.eq_ignore_ascii_case(&prefix)
    {
        return None;
    }
    if items.is_empty() {
        return None;
    }
    Some(Completion {
        title,
        token_start,
        prefix,
        items,
    })
}

/// The input after accepting suggestion `index`: the token is replaced and, when more arguments
/// are expected, a space is added.
pub fn accept(input: &str, completion: &Completion, index: usize) -> String {
    let Some(item) = completion
        .items
        .get(index.min(completion.items.len().saturating_sub(1)))
    else {
        return input.to_owned();
    };
    let mut text = input[..completion.token_start].to_owned();
    text.push_str(&item.insert);
    if item.more_args {
        text.push(' ');
    }
    text
}

/// The characters of suggestion `index` that are not typed yet (shown dimmed after the cursor).
pub fn ghost(completion: &Completion, index: usize) -> Option<String> {
    let item = completion.items.get(index)?;
    if !matches_prefix(&item.insert, &completion.prefix) {
        return None;
    }
    let rest = &item.insert[completion.prefix.len()..];
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// Whether `input` is already a complete, valid command: Enter then submits it instead of
/// accepting a suggestion.
pub fn is_complete(input: &str, ctx: &Context<'_>) -> bool {
    let Some(Ok(command)) = commands::parse(input) else {
        return false;
    };
    match command {
        SlashCommand::Model(Some(alias)) => ctx.models.iter().any(|m| m.alias == alias),
        SlashCommand::Effort(level) => ctx.current_model.is_some_and(|m| {
            m.allowed_efforts
                .iter()
                .any(|e| e.as_str().eq_ignore_ascii_case(&level))
        }),
        SlashCommand::Theme(Some(name)) => builtin::find(&name).is_some(),
        SlashCommand::Mcp(McpCommand::Connect(name) | McpCommand::Disconnect(name))
        | SlashCommand::Kb(KbCommand::Adopt(name)) => ctx.mcp.contains_key(&name),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use harness_agent::registry::Effort;

    use super::*;

    fn models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                alias: "luna".to_owned(),
                provider: "openai".to_owned(),
                allowed_efforts: vec![Effort::Low, Effort::High, Effort::Max],
                default_effort: Effort::High,
            },
            ModelInfo {
                alias: "flash".to_owned(),
                provider: "gemini".to_owned(),
                allowed_efforts: vec![Effort::Low, Effort::Medium],
                default_effort: Effort::Medium,
            },
        ]
    }

    fn server(name: &str, state: McpState, managed: bool) -> (String, McpServerStatus) {
        (
            name.to_owned(),
            McpServerStatus {
                name: name.to_owned(),
                state,
                live_tools: 3,
                managed,
                exposed: Vec::new(),
            },
        )
    }

    fn with_ctx<R>(f: impl FnOnce(&Context<'_>) -> R) -> R {
        let models = models();
        let mcp: BTreeMap<String, McpServerStatus> = [
            server("calc", McpState::Connected, false),
            server("docs", McpState::Disconnected, false),
        ]
        .into_iter()
        .collect();
        let ctx = Context {
            models: &models,
            current_model: models.first(),
            mcp: &mcp,
        };
        f(&ctx)
    }

    fn inserts(c: &Completion) -> Vec<&str> {
        c.items.iter().map(|i| i.insert.as_str()).collect()
    }

    #[test]
    fn a_lone_slash_lists_every_command_with_the_help_descriptions() {
        with_ctx(|ctx| {
            let c = suggest("/", ctx).expect("suggestions");
            assert_eq!(
                inserts(&c),
                [
                    "/model", "/effort", "/mcp", "/kb", "/drift", "/cost", "/theme", "/clear",
                    "/help", "/quit"
                ]
            );
            assert_eq!(c.items[0].detail, COMMAND_HELP[0].1, "reuses COMMAND_HELP");
            assert_eq!(c.title, "commands");
            assert_eq!((c.token_start, c.prefix.as_str()), (0, "/"));
        });
    }

    #[test]
    fn command_names_match_by_prefix_and_ignore_case() {
        with_ctx(|ctx| {
            let c = suggest("/m", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["/model", "/mcp"]);
            assert_eq!(
                inserts(&suggest("/MO", ctx).expect("suggestions")),
                ["/model"]
            );
            assert!(suggest("/zzz", ctx).is_none());
        });
    }

    #[test]
    fn commands_with_arguments_add_a_space_and_the_others_do_not() {
        with_ctx(|ctx| {
            let c = suggest("/mo", ctx).expect("suggestions");
            assert_eq!(accept("/mo", &c, 0), "/model ");
            let c = suggest("/he", ctx).expect("suggestions");
            assert_eq!(accept("/he", &c, 0), "/help");
            let c = suggest("/k", ctx).expect("suggestions");
            assert_eq!(accept("/k", &c, 0), "/kb ");
        });
    }

    #[test]
    fn paths_and_prose_do_not_trigger_suggestions() {
        with_ctx(|ctx| {
            assert!(suggest("/etc/hosts is broken", ctx).is_none());
            assert!(suggest("/etc/hosts", ctx).is_none());
            assert!(suggest("/foo bar", ctx).is_none());
            assert!(suggest("please /model", ctx).is_none());
            assert!(suggest("", ctx).is_none());
            assert!(suggest("/model\nsecond line", ctx).is_none());
            assert!(
                suggest("/help me with this", ctx).is_none(),
                "no args after /help"
            );
        });
    }

    #[test]
    fn a_fully_typed_command_without_arguments_has_nothing_to_suggest() {
        with_ctx(|ctx| {
            assert!(suggest("/cost", ctx).is_none());
            // But a command that takes arguments still shows itself, so Tab can move on.
            assert_eq!(inserts(&suggest("/model", ctx).expect("shown")), ["/model"]);
        });
    }

    #[test]
    fn model_aliases_come_from_the_registry() {
        with_ctx(|ctx| {
            let c = suggest("/model ", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["luna", "flash"]);
            assert_eq!(c.items[0].detail, "openai · efforts low, high, max");
            assert_eq!(c.token_start, "/model ".len());
            let c = suggest("/model fl", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["flash"]);
            assert_eq!(accept("/model fl", &c, 0), "/model flash");
            assert!(suggest("/model luna ", ctx).is_none(), "one argument only");
        });
    }

    #[test]
    fn effort_levels_come_from_the_current_model() {
        with_ctx(|ctx| {
            let c = suggest("/effort ", ctx).expect("suggestions");
            assert_eq!(
                inserts(&c),
                ["low", "high", "max"],
                "luna accepts these only"
            );
            assert_eq!(c.items[1].detail, "luna default");
            assert_eq!(c.items[0].detail, "for luna");
            let c = suggest("/effort h", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["high"]);
        });
    }

    #[test]
    fn mcp_completes_subcommands_then_server_names() {
        with_ctx(|ctx| {
            let c = suggest("/mcp ", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["connect", "disconnect"]);
            assert!(c.items.iter().all(|i| i.more_args));
            let c = suggest("/mcp conn", ctx).expect("suggestions");
            assert_eq!(accept("/mcp conn", &c, 0), "/mcp connect ");
            let c = suggest("/mcp connect ", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["calc", "docs"]);
            assert_eq!(c.items[0].detail, "connected, 3 tools, not in the KB");
            assert_eq!(c.items[1].detail, "disconnected");
            let c = suggest("/mcp disconnect d", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["docs"]);
            assert!(suggest("/mcp bogus ", ctx).is_none());
        });
    }

    #[test]
    fn kb_adopt_completes_server_names() {
        with_ctx(|ctx| {
            assert_eq!(
                inserts(&suggest("/kb ", ctx).expect("suggestions")),
                ["adopt"]
            );
            let c = suggest("/kb adopt ", ctx).expect("suggestions");
            assert_eq!(inserts(&c), ["calc", "docs"]);
            let c = suggest("/kb adopt ca", ctx).expect("suggestions");
            assert_eq!(accept("/kb adopt ca", &c, 0), "/kb adopt calc");
        });
    }

    #[test]
    fn ghost_text_is_the_rest_of_the_selected_suggestion() {
        with_ctx(|ctx| {
            let c = suggest("/mo", ctx).expect("suggestions");
            assert_eq!(ghost(&c, 0).as_deref(), Some("del"));
            let c = suggest("/m", ctx).expect("suggestions");
            assert_eq!(ghost(&c, 1).as_deref(), Some("cp"));
            let c = suggest("/model fl", ctx).expect("suggestions");
            assert_eq!(ghost(&c, 0).as_deref(), Some("ash"));
            let c = suggest("/model", ctx).expect("shown");
            assert_eq!(ghost(&c, 0), None, "nothing left to type");
        });
    }

    #[test]
    fn enter_submits_only_complete_valid_commands() {
        with_ctx(|ctx| {
            assert!(is_complete("/model", ctx), "opens the picker");
            assert!(is_complete("/model ", ctx));
            assert!(is_complete("/model luna", ctx));
            assert!(!is_complete("/model lu", ctx), "unknown alias");
            assert!(!is_complete("/mo", ctx), "unknown command");
            assert!(!is_complete("/effort", ctx), "missing level");
            assert!(is_complete("/effort max", ctx));
            assert!(
                !is_complete("/effort xhigh", ctx),
                "luna does not accept it"
            );
            assert!(is_complete("/mcp", ctx));
            assert!(!is_complete("/mcp connect nope", ctx));
            assert!(is_complete("/mcp connect calc", ctx));
            assert!(!is_complete("/kb adopt", ctx));
            assert!(is_complete("/help", ctx));
            assert!(!is_complete("hello", ctx));
        });
    }

    #[test]
    fn more_than_eight_commands_scroll_in_the_popup_not_here() {
        // The matcher returns every match; the popup shows MAX_ROWS and scrolls.
        with_ctx(|ctx| {
            assert!(suggest("/", ctx).expect("suggestions").items.len() > MAX_ROWS);
        });
    }
}
