//! Offline contract check for a Tool KB and returned JSONL log.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use harness_core::handoff::check_handoff;
use harness_core::redact::Redactor;

const USAGE: &str = "Usage: check_handoff --kb <tool-kb.json> --events <events.jsonl>";

fn paths() -> Result<Option<(PathBuf, PathBuf)>, &'static str> {
    let mut kb = None;
    let mut events = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        let target = if arg == "--kb" {
            &mut kb
        } else if arg == "--events" {
            &mut events
        } else {
            return Err("unknown argument");
        };
        if target.is_some() {
            return Err("duplicate option");
        }
        let value: OsString = args.next().ok_or("missing option value")?;
        if value.is_empty() || value == "--kb" || value == "--events" {
            return Err("missing option value");
        }
        *target = Some(PathBuf::from(value));
    }
    Ok(Some((
        kb.ok_or("--kb is required")?,
        events.ok_or("--events is required")?,
    )))
}

fn main() -> ExitCode {
    let (kb, events) = match paths() {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match check_handoff(kb, events) {
        Ok(summary) => match serde_json::to_string_pretty(&summary) {
            Ok(json) => {
                println!("{json}");
                println!(
                    "Contract checks passed. This does not establish task success, run completeness, or that the log used this Tool KB."
                );
                ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("cannot serialize the validation summary");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("{}", Redactor::default().redact_str(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}
