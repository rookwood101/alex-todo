//! Board MCP server (task 110).
//!
//! Rustboard exposes its board to Pi over a local stdio MCP server. The crate
//! is a library with thin binaries: src/bin/rustboard-mcp.rs parses --db (or
//! uses RUSTBOARD_HOME / %LOCALAPPDATA%\Rustboard), resolves a StorePaths and
//! hands it to server::BoardServer, which speaks MCP on stdin/stdout. Stdout is
//! protocol only; diagnostics go to stderr.
//!
//! The tool surface is:
//!
//! - board_schema: tables and columns, with the five audit tables marked
//!   read-only and notes/groups/edges marked read-write.
//! - board_query: one read-only statement on a SQLITE_OPEN_READ_ONLY +
//!   query_only connection, capped at 500 rows / 64 KiB.
//! - board_apply: typed Command values through Store::apply_with as actor
//!   agent, returning the batch id, effects and new ids.
//! - board_sql: one guarded DML/SELECT statement. The authorizer is installed
//!   only around the user's statement; the tool owns BEGIN IMMEDIATE, runs
//!   foreign-key, integrity, parent-cycle, edge-endpoint and time-zone checks
//!   with the authorizer cleared, appends a batch + whole-board event +
//!   before-image snapshot, and commits only if every check passed. Any
//!   failure rolls back.
//! - board_history: a note's events or a batch range, read-only.
//!
//! # Wiring for task 170
//!
//! Build the binary next to rustboard.exe (cargo build --bins puts both in
//! target/<profile>). Task 120's src/agent/mcp.rs already writes the project
//! config at <board_home>/.pi/mcp.json; the required entry shape is:
//!
//! <pre>
//! {
//!   "mcpServers": {
//!     "rustboard": {
//!       "command": "C:\\path\\to\\rustboard-mcp.exe",
//!       "args": ["--db", "C:\\path\\to\\board.sqlite3"],
//!       "exposure": "direct"
//!     }
//!   }
//! }
//! </pre>
//!
//! exposure must be "direct": with Pi's default deferred exposure the
//! --tools mcp__rustboard__* allowlist yields zero active tools (005-findings).
//! Pi then calls the tools as mcp__rustboard__<name> (board mode keeps the
//! shell-free allowlist). The database path belongs in args, never in the
//! child environment, so the server always opens exactly the board the app is
//! showing.
//!
//! # Manual protocol check
//!
//! With a migrated board, run rustboard-mcp --db <path> and write these lines
//! to stdin (one JSON object per line). The first response is initialize, the
//! second lists the five tools:
//!
//! <pre>
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"manual","version":"0.0.0"}}}
//! {"jsonrpc":"2.0","method":"notifications/initialized"}
//! {"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
//! </pre>

pub mod server;
pub mod sql;
pub mod tools;
