//! Agent loop, model clients, MCP client and tools for the MCP harness.
//!
//! This crate hosts the shared agent core used by the interactive TUI and the
//! benchmark command: provider clients for the OpenAI Responses API and the
//! Gemini Interactions API, an MCP client, built-in coding tools, an approval
//! policy and the loop that ties them together.

pub mod agent;
pub mod approval;
pub mod bridge;
pub mod drift;
pub mod events;
pub mod mcp;
pub mod model;
pub mod pricing;
pub mod registry;
pub mod tools;
pub mod transcript;
