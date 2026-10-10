//! Built-in coding tools against a temporary workspace.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use harness_agent::tools::BuiltinKind;
use harness_agent::tools::builtin::{BuiltinTools, ToolOutput};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Workspace {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    outside: std::path::PathBuf,
    tools: BuiltinTools,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonical");
    let root = base.join("ws");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).expect("ws");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.txt"), "TOP SECRET\n").expect("secret");
    let tools = BuiltinTools::new(&root, Vec::<String>::new()).expect("tools");
    Workspace {
        _dir: dir,
        root,
        outside,
        tools,
    }
}

async fn run(ws: &Workspace, kind: BuiltinKind, args: Value) -> ToolOutput {
    ws.tools.run(kind, &args, &CancellationToken::new()).await
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, text).expect("write");
}

#[tokio::test]
async fn builtin_read_file_numbers_lines_and_honours_offset_and_limit() {
    let ws = workspace();
    let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    write(&ws.root.join("a.txt"), &body);

    let all = run(&ws, BuiltinKind::ReadFile, json!({"path": "a.txt"})).await;
    assert!(!all.is_error, "{all:?}");
    assert!(all.text.starts_with("     1\tline 1\n"), "{}", all.text);
    assert!(all.text.contains("    10\tline 10"));
    assert!(!all.text.contains("truncated"));

    let page = run(
        &ws,
        BuiltinKind::ReadFile,
        json!({"path": "a.txt", "offset": 4, "limit": 3}),
    )
    .await;
    assert_eq!(
        page.text,
        "     4\tline 4\n     5\tline 5\n     6\tline 6\n[truncated: showing lines 4-6 of 10; call again with offset 7]"
    );

    let past = run(
        &ws,
        BuiltinKind::ReadFile,
        json!({"path": "a.txt", "offset": 99}),
    )
    .await;
    assert!(past.is_error);

    let missing = run(&ws, BuiltinKind::ReadFile, json!({"path": "nope.txt"})).await;
    assert!(missing.is_error);

    let bad_args = run(&ws, BuiltinKind::ReadFile, json!("not json at all")).await;
    assert!(bad_args.is_error);
    assert!(
        bad_args.text.contains("not valid JSON"),
        "{}",
        bad_args.text
    );
    let wrong_type = run(&ws, BuiltinKind::ReadFile, json!({"path": 5})).await;
    assert!(wrong_type.is_error);
}

#[tokio::test]
async fn builtin_read_file_refuses_files_over_10_mib() {
    let ws = workspace();
    let big = std::fs::File::create(ws.root.join("big.bin")).expect("create");
    big.set_len(10 * 1024 * 1024 + 1).expect("set_len");
    let out = run(&ws, BuiltinKind::ReadFile, json!({"path": "big.bin"})).await;
    assert!(out.is_error);
    assert!(out.text.contains("10 MiB"), "{}", out.text);
}

#[tokio::test]
async fn builtin_parent_dir_paths_outside_the_root_are_rejected() {
    let ws = workspace();
    for path in [
        "../outside/secret.txt",
        "sub/../../outside/secret.txt",
        "missing/../../outside/new.txt",
    ] {
        let read = run(&ws, BuiltinKind::ReadFile, json!({"path": path})).await;
        assert!(read.is_error, "{path}: {read:?}");
        assert!(
            read.text.contains("outside the workspace"),
            "{path}: {}",
            read.text
        );
        let wrote = run(
            &ws,
            BuiltinKind::WriteFile,
            json!({"path": path, "content": "pwned"}),
        )
        .await;
        assert!(wrote.is_error, "{path}: {wrote:?}");
    }
    assert!(!ws.outside.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(ws.outside.join("secret.txt")).unwrap(),
        "TOP SECRET\n"
    );
    // Absolute paths: outside is refused, inside is fine.
    let abs_outside = ws.outside.join("secret.txt").display().to_string();
    let out = run(&ws, BuiltinKind::ReadFile, json!({"path": abs_outside})).await;
    assert!(out.is_error);
    write(&ws.root.join("in.txt"), "hello\n");
    let abs_inside = ws.root.join("in.txt").display().to_string();
    let out = run(&ws, BuiltinKind::ReadFile, json!({"path": abs_inside})).await;
    assert!(!out.is_error, "{out:?}");
    // A `..` that stays inside is fine.
    let out = run(&ws, BuiltinKind::ReadFile, json!({"path": "x/../in.txt"})).await;
    assert!(!out.is_error, "{out:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn builtin_symlinks_pointing_outside_are_rejected() {
    use std::os::unix::fs::symlink;
    let ws = workspace();
    symlink(ws.outside.join("secret.txt"), ws.root.join("link.txt")).expect("file link");
    symlink(&ws.outside, ws.root.join("linkdir")).expect("dir link");
    symlink(ws.outside.join("not-yet.txt"), ws.root.join("dangling.txt")).expect("dangling");

    let read = run(&ws, BuiltinKind::ReadFile, json!({"path": "link.txt"})).await;
    assert!(read.is_error, "{read:?}");
    assert!(!read.text.contains("TOP SECRET"));

    let read_via_dir = run(
        &ws,
        BuiltinKind::ReadFile,
        json!({"path": "linkdir/secret.txt"}),
    )
    .await;
    assert!(read_via_dir.is_error, "{read_via_dir:?}");

    let write_via_dir = run(
        &ws,
        BuiltinKind::WriteFile,
        json!({"path": "linkdir/created.txt", "content": "x"}),
    )
    .await;
    assert!(write_via_dir.is_error, "{write_via_dir:?}");
    assert!(!ws.outside.join("created.txt").exists());

    let write_dangling = run(
        &ws,
        BuiltinKind::WriteFile,
        json!({"path": "dangling.txt", "content": "x"}),
    )
    .await;
    assert!(write_dangling.is_error, "{write_dangling:?}");
    assert!(!ws.outside.join("not-yet.txt").exists());

    let edit = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "link.txt", "old_string": "TOP", "new_string": "LOW"}),
    )
    .await;
    assert!(edit.is_error);
    assert_eq!(
        std::fs::read_to_string(ws.outside.join("secret.txt")).unwrap(),
        "TOP SECRET\n"
    );

    // A symlink that stays inside the workspace is fine.
    write(&ws.root.join("real.txt"), "inside\n");
    symlink(ws.root.join("real.txt"), ws.root.join("ok-link.txt")).expect("inner link");
    let ok = run(&ws, BuiltinKind::ReadFile, json!({"path": "ok-link.txt"})).await;
    assert!(!ok.is_error, "{ok:?}");
}

#[tokio::test]
async fn builtin_write_file_creates_parents_and_overwrites() {
    let ws = workspace();
    let out = run(
        &ws,
        BuiltinKind::WriteFile,
        json!({"path": "deep/er/new.txt", "content": "one\n"}),
    )
    .await;
    assert!(!out.is_error, "{out:?}");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("deep/er/new.txt")).unwrap(),
        "one\n"
    );
    let out = run(
        &ws,
        BuiltinKind::WriteFile,
        json!({"path": "deep/er/new.txt", "content": "two\n"}),
    )
    .await;
    assert!(!out.is_error);
    assert_eq!(
        std::fs::read_to_string(ws.root.join("deep/er/new.txt")).unwrap(),
        "two\n"
    );
    let dir_target = run(
        &ws,
        BuiltinKind::WriteFile,
        json!({"path": "deep", "content": "x"}),
    )
    .await;
    assert!(dir_target.is_error);
}

#[tokio::test]
async fn builtin_edit_file_requires_exactly_one_match_unless_replace_all() {
    let ws = workspace();
    write(&ws.root.join("e.txt"), "alpha beta alpha\n");

    let none = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "e.txt", "old_string": "gamma", "new_string": "x"}),
    )
    .await;
    assert!(none.is_error);
    assert!(none.text.contains("not found"), "{}", none.text);

    let two = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "e.txt", "old_string": "alpha", "new_string": "x"}),
    )
    .await;
    assert!(two.is_error);
    assert!(two.text.contains("2 times"), "{}", two.text);
    assert_eq!(
        std::fs::read_to_string(ws.root.join("e.txt")).unwrap(),
        "alpha beta alpha\n",
        "a failed edit must not change the file"
    );

    let one = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "e.txt", "old_string": "beta", "new_string": "BETA"}),
    )
    .await;
    assert!(!one.is_error, "{one:?}");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("e.txt")).unwrap(),
        "alpha BETA alpha\n"
    );

    let all = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "e.txt", "old_string": "alpha", "new_string": "A", "replace_all": true}),
    )
    .await;
    assert!(!all.is_error, "{all:?}");
    assert_eq!(all.text, "Replaced 2 occurrences in e.txt");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("e.txt")).unwrap(),
        "A BETA A\n"
    );

    let empty = run(
        &ws,
        BuiltinKind::EditFile,
        json!({"path": "e.txt", "old_string": "", "new_string": "x"}),
    )
    .await;
    assert!(empty.is_error);
}

#[tokio::test]
async fn builtin_list_dir_marks_directories_skips_git_and_supports_glob() {
    let ws = workspace();
    write(&ws.root.join("src/lib.rs"), "");
    write(&ws.root.join("src/deep/mod.rs"), "");
    write(&ws.root.join("README.md"), "");
    write(&ws.root.join(".git/config"), "");

    let top = run(&ws, BuiltinKind::ListDir, json!({})).await;
    assert_eq!(top.text, "README.md\nsrc/");

    let sub = run(&ws, BuiltinKind::ListDir, json!({"path": "src"})).await;
    assert_eq!(sub.text, "deep/\nlib.rs");

    let globbed = run(&ws, BuiltinKind::ListDir, json!({"glob": "*.rs"})).await;
    assert_eq!(globbed.text, "src/deep/mod.rs\nsrc/lib.rs");

    let none = run(&ws, BuiltinKind::ListDir, json!({"glob": "*.zzz"})).await;
    assert_eq!(none.text, "[no entries]");

    let bad = run(&ws, BuiltinKind::ListDir, json!({"glob": "[unclosed"})).await;
    assert!(bad.is_error);
    let not_dir = run(&ws, BuiltinKind::ListDir, json!({"path": "README.md"})).await;
    assert!(not_dir.is_error);
}

#[tokio::test]
async fn builtin_list_dir_caps_entries_at_1000() {
    let ws = workspace();
    for n in 0..1005 {
        write(&ws.root.join(format!("many/f{n:04}.txt")), "");
    }
    let out = run(&ws, BuiltinKind::ListDir, json!({"path": "many"})).await;
    assert!(!out.is_error);
    assert_eq!(out.text.lines().count(), 1001, "1000 entries plus the note");
    assert!(
        out.text
            .ends_with("[truncated: showing 1000 of 1005 entries]"),
        "{}",
        out.text.lines().last().unwrap_or_default()
    );
}

#[tokio::test]
async fn builtin_grep_respects_gitignore_glob_and_match_cap() {
    let ws = workspace();
    write(&ws.root.join(".gitignore"), "ignored/\n");
    write(&ws.root.join("a.rs"), "fn needle() {}\nother\n");
    write(&ws.root.join("b.txt"), "a needle here\n");
    write(&ws.root.join("ignored/c.rs"), "needle in ignored\n");
    write(&ws.root.join(".git/HEAD"), "needle in git\n");

    let out = run(&ws, BuiltinKind::Grep, json!({"pattern": "needle"})).await;
    assert!(!out.is_error, "{out:?}");
    let mut lines: Vec<_> = out.text.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        vec!["a.rs:1: fn needle() {}", "b.txt:1: a needle here"]
    );

    let only_rs = run(
        &ws,
        BuiltinKind::Grep,
        json!({"pattern": "needle", "glob": "*.rs"}),
    )
    .await;
    assert_eq!(only_rs.text.trim_end(), "a.rs:1: fn needle() {}");

    let scoped = run(
        &ws,
        BuiltinKind::Grep,
        json!({"pattern": "needle", "path": "b.txt"}),
    )
    .await;
    assert_eq!(scoped.text.trim_end(), "b.txt:1: a needle here");

    let none = run(&ws, BuiltinKind::Grep, json!({"pattern": "zzzzzz"})).await;
    assert_eq!(none.text, "[no matches]");

    let bad = run(&ws, BuiltinKind::Grep, json!({"pattern": "("})).await;
    assert!(bad.is_error);

    let outside = run(
        &ws,
        BuiltinKind::Grep,
        json!({"pattern": "SECRET", "path": "../outside"}),
    )
    .await;
    assert!(outside.is_error);

    let many: String = (0..300).map(|n| format!("hit {n}\n")).collect();
    write(&ws.root.join("many.log"), &many);
    let capped = run(
        &ws,
        BuiltinKind::Grep,
        json!({"pattern": "^hit ", "path": "many.log"}),
    )
    .await;
    assert_eq!(
        capped.text.lines().count(),
        201,
        "200 matches plus the note"
    );
    assert!(capped.text.ends_with("[truncated: more than 200 matches]"));
}

#[tokio::test]
async fn builtin_shell_reports_exit_code_and_streams() {
    let ws = workspace();
    write(&ws.root.join("marker.txt"), "");
    let ok = run(
        &ws,
        BuiltinKind::RunShell,
        json!({"command": "echo out; echo err >&2; ls"}),
    )
    .await;
    assert!(!ok.is_error, "{ok:?}");
    assert!(ok.text.starts_with("exit code: 0\n"), "{}", ok.text);
    assert!(
        ok.text.contains("--- stdout ---\nout\nmarker.txt"),
        "{}",
        ok.text
    );
    assert!(ok.text.contains("--- stderr ---\nerr"), "{}", ok.text);

    let failing = run(&ws, BuiltinKind::RunShell, json!({"command": "exit 3"})).await;
    assert!(failing.is_error);
    assert!(failing.text.starts_with("exit code: 3"), "{}", failing.text);

    // The command runs in the workspace root.
    let pwd = run(&ws, BuiltinKind::RunShell, json!({"command": "pwd"})).await;
    assert!(
        pwd.text.contains(&ws.root.display().to_string()),
        "{}",
        pwd.text
    );
}

#[tokio::test]
async fn builtin_shell_truncates_each_stream_at_30000_bytes() {
    let ws = workspace();
    let out = run(
        &ws,
        BuiltinKind::RunShell,
        json!({"command": "head -c 100000 /dev/zero | tr '\\0' 'a'; head -c 40000 /dev/zero | tr '\\0' 'b' >&2"}),
    )
    .await;
    assert!(!out.is_error, "{out:?}");
    assert!(out.text.contains("[truncated 70000 bytes]"), "stdout note");
    assert!(out.text.contains("[truncated 10000 bytes]"), "stderr note");
    // Exactly 30000 bytes of each stream are kept (the notes add other letters, so look for runs).
    assert!(out.text.contains(&"a".repeat(30_000)));
    assert!(!out.text.contains(&"a".repeat(30_001)));
    assert!(out.text.contains(&"b".repeat(30_000)));
    assert!(!out.text.contains(&"b".repeat(30_001)));
}

#[tokio::test]
async fn builtin_shell_timeout_kills_the_process() {
    let ws = workspace();
    let marker = ws.root.join("should-not-exist");
    let started = Instant::now();
    let out = run(
        &ws,
        BuiltinKind::RunShell,
        json!({"command": "sleep 20; touch should-not-exist", "timeout_secs": 1}),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "returned promptly"
    );
    assert!(out.is_error);
    assert!(out.text.contains("timed out after 1s"), "{}", out.text);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "the killed command must not finish");
}

#[tokio::test]
async fn builtin_shell_cancellation_kills_the_process() {
    let ws = workspace();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let out = ws
        .tools
        .run(
            BuiltinKind::RunShell,
            &json!({"command": "sleep 20"}),
            &cancel,
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(out.is_error);
    assert!(out.text.contains("cancelled"), "{}", out.text);
}

#[tokio::test]
async fn builtin_shell_env_is_scrubbed_of_named_variables() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Control: a variable that cargo sets for every test run is visible without scrubbing.
    // (Setting a variable here would need `unsafe`, which the workspace forbids.)
    let Ok(name) = std::env::var("CARGO_PKG_NAME") else {
        eprintln!("not running under cargo; skipping env scrub test");
        return;
    };
    let probe = json!({"command": "printf '%s' \"${CARGO_PKG_NAME-unset}\""});
    let open = BuiltinTools::new(dir.path(), Vec::<String>::new()).expect("tools");
    let seen = open
        .run(BuiltinKind::RunShell, &probe, &CancellationToken::new())
        .await;
    assert!(seen.text.contains(&name), "control: {}", seen.text);

    let scrubbing =
        BuiltinTools::new(dir.path(), vec!["CARGO_PKG_NAME".to_owned()]).expect("tools");
    let hidden = scrubbing
        .run(BuiltinKind::RunShell, &probe, &CancellationToken::new())
        .await;
    assert!(hidden.text.contains("unset"), "{}", hidden.text);
    assert!(!hidden.text.contains(&name), "{}", hidden.text);

    // The real API key variables from the registry are scrubbed whether or not they are set
    // in the environment running the tests. The probe prints only present/absent.
    let registry = common::registry();
    let tools = BuiltinTools::new(dir.path(), registry.api_key_envs()).expect("tools");
    let out = tools
        .run(
            BuiltinKind::RunShell,
            &json!({"command": "[ -z \"${OPENAI_API_KEY+x}${GEMINI_API_KEY+x}\" ] && echo absent || echo present"}),
            &CancellationToken::new(),
        )
        .await;
    assert!(out.text.contains("absent"), "{}", out.text);
    assert!(!out.text.contains("present"), "{}", out.text);
}
