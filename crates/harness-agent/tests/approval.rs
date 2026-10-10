//! Approval policy and diff rendering.

mod common;

use harness_agent::approval::{
    ApprovalClass, ApprovalPolicy, DENIED_TEXT, Decision, PolicyAction, unified_diff,
};
use harness_agent::tools::BuiltinKind;
use harness_agent::tools::builtin::{BuiltinTools, approval_class};
use serde_json::json;

#[test]
fn approval_defaults_ask_for_write_and_shell_only() {
    let policy = ApprovalPolicy::default();
    assert!(!policy.requires_approval(ApprovalClass::Read));
    assert!(policy.requires_approval(ApprovalClass::Write));
    assert!(policy.requires_approval(ApprovalClass::Shell));
    assert!(!policy.requires_approval(ApprovalClass::Mcp));
    assert_eq!(
        policy.default_action(ApprovalClass::Write),
        PolicyAction::Ask
    );
}

#[test]
fn approval_session_decision_stops_later_prompts_for_that_class_only() {
    let mut policy = ApprovalPolicy::default();
    assert!(policy.requires_approval(ApprovalClass::Write));
    // Once allows this call but keeps asking.
    assert!(policy.record(ApprovalClass::Write, Decision::Once));
    assert!(policy.requires_approval(ApprovalClass::Write));
    // Session allows it and every later call of the class.
    assert!(policy.record(ApprovalClass::Write, Decision::Session));
    assert!(!policy.requires_approval(ApprovalClass::Write));
    assert!(
        policy.requires_approval(ApprovalClass::Shell),
        "other classes still ask"
    );
    assert!(policy.session_allowed().contains(&ApprovalClass::Write));
}

#[test]
fn approval_deny_refuses_and_changes_nothing() {
    let mut policy = ApprovalPolicy::default();
    assert!(!policy.record(ApprovalClass::Shell, Decision::Deny));
    assert!(policy.requires_approval(ApprovalClass::Shell));
    assert!(policy.session_allowed().is_empty());
    assert_eq!(DENIED_TEXT, "The user denied this tool call.");
}

#[test]
fn approval_allow_all_and_overrides() {
    let all = ApprovalPolicy::allow_all();
    for class in [
        ApprovalClass::Read,
        ApprovalClass::Write,
        ApprovalClass::Shell,
        ApprovalClass::Mcp,
    ] {
        assert!(!all.requires_approval(class));
    }
    let strict = ApprovalPolicy::default().with_default(ApprovalClass::Mcp, PolicyAction::Ask);
    assert!(strict.requires_approval(ApprovalClass::Mcp));
}

#[test]
fn approval_classes_of_builtin_tools() {
    assert_eq!(approval_class(BuiltinKind::ReadFile), ApprovalClass::Read);
    assert_eq!(approval_class(BuiltinKind::ListDir), ApprovalClass::Read);
    assert_eq!(approval_class(BuiltinKind::Grep), ApprovalClass::Read);
    assert_eq!(approval_class(BuiltinKind::WriteFile), ApprovalClass::Write);
    assert_eq!(approval_class(BuiltinKind::EditFile), ApprovalClass::Write);
    assert_eq!(approval_class(BuiltinKind::RunShell), ApprovalClass::Shell);
}

#[test]
fn approval_unified_diff_renders_hunks_with_context() {
    let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\nj\n";
    assert_eq!(
        unified_diff("f.txt", old, new),
        "--- a/f.txt\n+++ b/f.txt\n@@ -2,7 +2,7 @@\n b\n c\n d\n-e\n+E\n f\n g\n h\n"
    );
    assert_eq!(unified_diff("f.txt", old, old), "");
    // New file: everything is an addition.
    assert_eq!(
        unified_diff("n.txt", "", "x\ny\n"),
        "--- a/n.txt\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"
    );
    // Two distant changes give two hunks.
    let far_old: String = (1..=30).map(|n| format!("{n}\n")).collect();
    let far_new = far_old
        .replacen("2\n", "two\n", 1)
        .replacen("29\n", "twenty-nine\n", 1);
    let diff = unified_diff("g.txt", &far_old, &far_new);
    assert_eq!(diff.matches("\n@@ ").count(), 2, "{diff}");
    assert!(
        diff.contains("-2\n+two\n") && diff.contains("-29\n+twenty-nine\n"),
        "{diff}"
    );
}

#[test]
fn approval_preview_shows_a_diff_for_write_and_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tools = BuiltinTools::new(dir.path(), Vec::<String>::new()).expect("tools");
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").expect("write");

    let write = tools.preview(
        BuiltinKind::WriteFile,
        &json!({"path": "f.txt", "content": "one\nTWO\n"}),
    );
    assert_eq!(write.summary, "Overwrite f.txt (8 bytes)");
    assert_eq!(
        write.diff.as_deref(),
        Some("--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n")
    );

    let create = tools.preview(
        BuiltinKind::WriteFile,
        &json!({"path": "new.txt", "content": "hi\n"}),
    );
    assert!(
        create.summary.starts_with("Create new.txt"),
        "{}",
        create.summary
    );

    let edit = tools.preview(
        BuiltinKind::EditFile,
        &json!({"path": "f.txt", "old_string": "two", "new_string": "2"}),
    );
    assert_eq!(edit.summary, "Edit f.txt");
    assert!(edit.diff.expect("diff").contains("-two\n+2\n"));

    let shell = tools.preview(BuiltinKind::RunShell, &json!({"command": "ls -la"}));
    assert_eq!(shell.summary, "Run: ls -la");
    assert!(shell.diff.is_none());

    let broken = tools.preview(BuiltinKind::WriteFile, &json!({"path": 5}));
    assert!(broken.diff.is_none());
}
