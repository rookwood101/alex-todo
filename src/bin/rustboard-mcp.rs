//! rustboard-mcp: stdio MCP server for the Rustboard board (task 110).
//!
//! Usage: rustboard-mcp [--db <path>]
//!
//! Without --db the board home comes from RUSTBOARD_HOME, then
//! %LOCALAPPDATA%\Rustboard. Stdout carries MCP protocol frames only; logs and
//! fatal start-up errors go to stderr. A missing or invalid database is
//! reported by every tool as an MCP error instead of panicking the process.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use rmcp::ServiceExt;
use rustboard::mcp::server::BoardServer;
use rustboard::store::StorePaths;

const USAGE: &str = "usage: rustboard-mcp [--db <board.sqlite3>]\n\
\n\
Serves the five Rustboard board tools over stdio MCP. Without --db the board\n\
home is RUSTBOARD_HOME, then %LOCALAPPDATA%\\Rustboard.\n";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rustboard-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let db = match parse_args(std::env::args_os().skip(1))? {
        Cli::Run(db) => db,
        Cli::Help => return Ok(()),
    };
    let paths = StorePaths::resolve(db.as_deref())
        .map_err(|error| format!("cannot resolve the board location: {error}"))?;
    eprintln!(
        "rustboard-mcp: serving stdio MCP for {}",
        paths.db().display()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the async runtime: {error}"))?;
    runtime.block_on(serve(paths))
}

async fn serve(paths: StorePaths) -> Result<(), String> {
    let server = BoardServer::new(paths);
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| format!("cannot start the MCP transport: {error}"))?;
    running
        .waiting()
        .await
        .map_err(|error| format!("MCP transport stopped: {error}"))?;
    Ok(())
}

enum Cli {
    Run(Option<PathBuf>),
    Help,
}

/// Parse --db, -h/--help. Help means the text was printed and the process
/// should exit successfully.
fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Cli, String> {
    let mut db: Option<PathBuf> = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        let Some(text) = argument.to_str() else {
            return Err("arguments must be valid UTF-8".to_owned());
        };
        match text {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(Cli::Help);
            }
            "--db" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--db needs a path argument".to_owned())?;
                db = Some(PathBuf::from(value));
            }
            _ if text.starts_with("--db=") => {
                let value = &text["--db=".len()..];
                if value.is_empty() {
                    return Err("--db needs a path argument".to_owned());
                }
                db = Some(PathBuf::from(value));
            }
            other => {
                return Err(format!("unknown argument {other:?}\n\n{USAGE}"));
            }
        }
    }
    Ok(Cli::Run(db))
}
