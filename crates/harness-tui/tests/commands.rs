//! Slash command parsing.

use harness_tui::commands::{KbCommand, McpCommand, SlashCommand, parse};

fn ok(line: &str) -> SlashCommand {
    parse(line)
        .unwrap_or_else(|| panic!("{line:?} should be a command"))
        .unwrap_or_else(|e| panic!("{line:?} should parse, got error: {e}"))
}

fn err(line: &str) -> String {
    parse(line)
        .unwrap_or_else(|| panic!("{line:?} should be a command"))
        .expect_err("should be an error")
}

#[test]
fn every_command_row_parses() {
    assert_eq!(ok("/model"), SlashCommand::Model(None));
    assert_eq!(
        ok("/model sol"),
        SlashCommand::Model(Some("sol".to_owned()))
    );
    assert_eq!(ok("/effort high"), SlashCommand::Effort("high".to_owned()));
    assert_eq!(ok("/mcp"), SlashCommand::Mcp(McpCommand::List));
    assert_eq!(
        ok("/mcp connect calc"),
        SlashCommand::Mcp(McpCommand::Connect("calc".to_owned()))
    );
    assert_eq!(
        ok("/mcp disconnect calc"),
        SlashCommand::Mcp(McpCommand::Disconnect("calc".to_owned()))
    );
    assert_eq!(ok("/kb"), SlashCommand::Kb(KbCommand::Show));
    assert_eq!(
        ok("/kb adopt calc"),
        SlashCommand::Kb(KbCommand::Adopt("calc".to_owned()))
    );
    assert_eq!(ok("/drift"), SlashCommand::Drift);
    assert_eq!(ok("/cost"), SlashCommand::Cost);
    assert_eq!(ok("/clear"), SlashCommand::Clear);
    assert_eq!(ok("/help"), SlashCommand::Help);
    assert_eq!(ok("/quit"), SlashCommand::Quit);
    assert_eq!(ok("/exit"), SlashCommand::Quit);
}

#[test]
fn whitespace_is_tolerated() {
    assert_eq!(
        ok("  /model    sol  "),
        SlashCommand::Model(Some("sol".to_owned()))
    );
}

#[test]
fn bad_arguments_give_a_usage_message() {
    assert!(err("/model a b").contains("usage: /model"));
    assert!(err("/effort").contains("usage: /effort"));
    assert!(err("/mcp connect").contains("usage: /mcp"));
    assert!(err("/mcp restart calc").contains("usage: /mcp"));
    assert!(err("/kb adopt").contains("usage: /kb"));
    assert!(err("/cost now").contains("takes no arguments"));
}

#[test]
fn unknown_commands_are_errors_not_messages() {
    let message = err("/frobnicate");
    assert!(message.contains("unknown command /frobnicate"), "{message}");
    assert!(message.contains("/help"), "{message}");
}

#[test]
fn ordinary_text_and_paths_are_not_commands() {
    assert!(parse("hello there").is_none());
    assert!(parse("").is_none());
    assert!(parse("   ").is_none());
    assert!(parse("/etc/hosts is missing").is_none());
    assert!(parse("/").is_none());
    assert!(parse("please run /model").is_none());
}
