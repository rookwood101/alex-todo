//! Rustboard library: the board model, SQLite storage, commands, and UI.
//!
//! The three binaries (`rustboard`, `todo`, `rustboard-mcp`) are thin shells over
//! this crate so the command layer and the MCP server always agree on types.

pub mod agent;
pub mod app;
pub mod camera;
pub mod capture;
pub mod command;
pub mod config;
#[cfg(test)]
mod headless_tests;
pub mod history;
pub mod layout;
pub mod mcp;
pub mod model;
pub mod notify;
pub mod platform;
pub mod remind;
pub mod store;
pub mod theme;
pub mod time;
pub mod ui;
