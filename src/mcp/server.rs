//! The rmcp stdio server surface for Rustboard (task 110).
//!
//! This module is deliberately thin: every tool decodes its arguments, calls
//! one plain function in super::tools, and serializes the result. The
//! database work is synchronous inside the async handler; a rusqlite
//! connection is never held across an .await.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::Tool;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
use serde_json::Value;

use crate::store::StorePaths;

use super::tools::{self, BoardApplyArgs, BoardHistoryArgs, BoardQueryArgs, BoardSqlArgs};

/// One Rustboard MCP server bound to a board database.
#[derive(Clone)]
pub struct BoardServer {
    paths: StorePaths,
    tool_router: ToolRouter<Self>,
}

impl BoardServer {
    /// Bind the server to a resolved board location.
    pub fn new(paths: StorePaths) -> Self {
        Self {
            paths,
            tool_router: Self::tool_router(),
        }
    }

    /// The tool definitions this server advertises, sorted by name by the
    /// router. Used by the unit test that pins the contract and by callers
    /// that want to inspect capabilities without a transport.
    pub fn tool_definitions(&self) -> Vec<Tool> {
        self.tool_router.list_all()
    }
}

fn to_json<T: Serialize>(value: T) -> Result<Json<Value>, String> {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|error| format!("cannot encode tool result: {error}"))
}

#[tool_router]
impl BoardServer {
    /// List the board tables and their columns.
    #[tool(
        description = "List the Rustboard tables and their columns. The five audit tables (batches, undo_targets, events, snapshots, schema_migrations) are read-only; notes, groups and edges are read-write."
    )]
    async fn board_schema(&self) -> Result<Json<Value>, String> {
        to_json(tools::board_schema(&self.paths).map_err(|error| error.to_string())?)
    }

    /// Run one read-only SQL statement.
    #[tool(
        description = "Run one read-only SQL statement (SELECT, WITH, VALUES or EXPLAIN) against the board. Returns columns, rows and a truncated flag. At most 500 rows and 64 KiB are returned; the limit argument is clamped to 1..=500. Writes, pragmas and transaction control are rejected."
    )]
    async fn board_query(
        &self,
        Parameters(args): Parameters<BoardQueryArgs>,
    ) -> Result<Json<Value>, String> {
        to_json(tools::board_query(&self.paths, args).map_err(|error| error.to_string())?)
    }

    /// Apply typed commands as one agent batch.
    #[tool(
        description = "Apply one or more typed board commands as a single agent batch (the same command layer the UI uses). Returns the batch id, per-command effects and any new note/group/edge ids. Use this for normal board edits; it validates every command and records history automatically."
    )]
    async fn board_apply(
        &self,
        Parameters(args): Parameters<BoardApplyArgs>,
    ) -> Result<Json<Value>, String> {
        to_json(tools::board_apply(&self.paths, args).map_err(|error| error.to_string())?)
    }

    /// Run one audited DML statement.
    #[tool(
        description = "Run exactly one DML or SELECT statement under the board_sql authorizer. This is DML-only and audited: INSERT/UPDATE/DELETE are allowed on notes, groups and edges only, and the five audit tables plus the schema, pragmas, ATTACH, transactions, VACUUM and dangerous functions are rejected. A successful statement commits one apply batch, one whole-board event and a before-image snapshot; foreign-key, integrity, parent-cycle, edge-endpoint and time-zone checks must all pass or the whole statement rolls back."
    )]
    async fn board_sql(
        &self,
        Parameters(args): Parameters<BoardSqlArgs>,
    ) -> Result<Json<Value>, String> {
        to_json(tools::board_sql(&self.paths, args).map_err(|error| error.to_string())?)
    }

    /// Read note events or a batch range.
    #[tool(
        description = "Read history. With note_id, return that note's events in write order. Without note_id, return the batches created in the optional from_utc_ms/to_utc_ms range (epoch milliseconds UTC; defaults 0 and now). This path is read-only."
    )]
    async fn board_history(
        &self,
        Parameters(args): Parameters<BoardHistoryArgs>,
    ) -> Result<Json<Value>, String> {
        to_json(tools::board_history(&self.paths, args).map_err(|error| error.to_string())?)
    }
}

#[tool_handler(
    name = "rustboard",
    version = "0.1.0",
    instructions = "Rustboard board tools. board_sql is DML-only and audited: it cannot change the schema or the append-only audit tables, and every successful statement records a batch, a whole-board event and a snapshot. Prefer board_apply for typed edits."
)]
impl ServerHandler for BoardServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StorePaths, schema, scratch_dir};

    fn board(name: &str) -> StorePaths {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("board opens");
        paths
    }

    #[test]
    fn the_router_lists_the_five_contract_tools_with_schemas() {
        let server = BoardServer::new(board("mcp-server-router"));
        let tools = server.tool_definitions();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "board_apply",
                "board_history",
                "board_query",
                "board_schema",
                "board_sql"
            ]
        );
        for tool in &tools {
            assert!(
                tool.description
                    .as_deref()
                    .is_some_and(|description| !description.is_empty()),
                "{} needs a description",
                tool.name
            );
        }

        let query = tools
            .iter()
            .find(|tool| tool.name == "board_query")
            .expect("board_query");
        let properties = query
            .input_schema
            .get("properties")
            .expect("board_query properties");
        assert!(properties.get("sql").is_some());
        assert!(properties.get("limit").is_some());

        let sql = tools
            .iter()
            .find(|tool| tool.name == "board_sql")
            .expect("board_sql");
        let description = sql.description.as_deref().expect("board_sql description");
        assert!(description.contains("DML-only"), "{description}");
        assert!(description.contains("audited"), "{description}");
        let properties = sql.input_schema.get("properties").expect("sql properties");
        assert!(properties.get("sql").is_some());
        assert!(properties.get("description").is_some());

        let apply = tools
            .iter()
            .find(|tool| tool.name == "board_apply")
            .expect("board_apply");
        let properties = apply
            .input_schema
            .get("properties")
            .expect("apply properties");
        let commands = properties.get("commands").expect("commands schema");
        assert_eq!(commands.get("type"), Some(&serde_json::json!("array")));
        let variants = commands
            .get("items")
            .and_then(|items| items.get("oneOf"))
            .and_then(serde_json::Value::as_array)
            .expect("command variants");
        assert_eq!(variants.len(), 19, "every Command variant is described");
        assert!(properties.get("description").is_some());
        assert!(properties.get("actor_ref").is_some());

        let history = tools
            .iter()
            .find(|tool| tool.name == "board_history")
            .expect("board_history");
        let properties = history
            .input_schema
            .get("properties")
            .expect("history properties");
        assert!(properties.get("note_id").is_some());
        assert!(properties.get("from_utc_ms").is_some());
        assert!(properties.get("to_utc_ms").is_some());
    }
}
