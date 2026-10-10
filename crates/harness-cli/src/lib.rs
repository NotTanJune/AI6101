//! Library half of the `harness` binary: command implementations.
//!
//! `validate` and `run` drive `harness_core::pipeline`; nothing here implements pipeline stages.
//! `tui` builds the coding agent from `harness-agent` and hands it to `harness-tui`.

pub mod commands;
pub mod launcher;
