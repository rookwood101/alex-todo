//! Pi MCP registration for the board database.
//!
//! Pi loads project MCP servers from `<board_home>/.pi/mcp.json`. The entry must
//! use `exposure: direct`: with the default deferred exposure,
//! `--tools mcp__rustboard__*` yields zero active tools (005).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use super::AgentError;

/// Project-level Pi config directory.
pub const MCP_DIR: &str = ".pi";
/// Pi MCP server file.
pub const MCP_FILE: &str = "mcp.json";
/// Server key referenced by the board tool pattern.
pub const SERVER_NAME: &str = "rustboard";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Write `<board_home>/.pi/mcp.json` with a direct-exposure `rustboard` server.
///
/// The MCP command is the `rustboard-mcp` binary next to the current
/// executable, and the database path is passed in `args` (never the
/// environment). Existing unrelated servers and top-level keys are preserved.
/// The file is written atomically and the call is idempotent.
pub fn ensure_mcp_config(board_home: &Path, db_path: &Path) -> Result<PathBuf, AgentError> {
    let command = mcp_command_path()?;
    ensure_mcp_config_with_command(board_home, db_path, &command)
}

fn ensure_mcp_config_with_command(
    board_home: &Path,
    db_path: &Path,
    command: &Path,
) -> Result<PathBuf, AgentError> {
    let command = command.to_str().ok_or_else(|| {
        AgentError::Mcp(format!(
            "rustboard-mcp path is not valid UTF-8: {}",
            command.display()
        ))
    })?;
    let db = db_path.to_str().ok_or_else(|| {
        AgentError::Mcp(format!(
            "database path is not valid UTF-8: {}",
            db_path.display()
        ))
    })?;

    let dir = board_home.join(MCP_DIR);
    fs::create_dir_all(&dir)
        .map_err(|error| AgentError::Mcp(format!("cannot create {}: {error}", dir.display())))?;
    let path = dir.join(MCP_FILE);

    let original = read_config(&path)?;
    let Value::Object(mut config) = original.clone() else {
        return Err(AgentError::Mcp(format!(
            "{} must contain a JSON object",
            path.display()
        )));
    };
    let entry = json!({
        "command": command,
        "args": ["--db", db],
        "exposure": "direct",
    });

    let servers = config
        .entry("mcpServers".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers.as_object_mut().ok_or_else(|| {
        AgentError::Mcp(format!(
            "{} has a non-object mcpServers value; refusing to overwrite it",
            path.display()
        ))
    })?;
    servers.insert(SERVER_NAME.to_owned(), entry);

    let updated = Value::Object(config);
    if updated == original {
        return Ok(path);
    }

    let mut bytes = serde_json::to_vec_pretty(&updated)?;
    bytes.push(b'\n');
    atomic_write(&path, &bytes)?;
    Ok(path)
}

/// Where rustboard-mcp is expected: next to the running executable.
pub fn mcp_command_path() -> Result<PathBuf, AgentError> {
    let executable = std::env::current_exe().map_err(|error| {
        AgentError::Mcp(format!("cannot locate the current executable: {error}"))
    })?;
    let name = if cfg!(windows) {
        "rustboard-mcp.exe"
    } else {
        "rustboard-mcp"
    };
    Ok(executable.with_file_name(name))
}

fn read_config(path: &Path) -> Result<Value, AgentError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Object(Map::new()));
        }
        Err(error) => {
            return Err(AgentError::Mcp(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        AgentError::Mcp(format!(
            "{} is not valid JSON; refusing to overwrite it: {error}",
            path.display()
        ))
    })?;
    if !value.is_object() {
        return Err(AgentError::Mcp(format!(
            "{} must contain a JSON object",
            path.display()
        )));
    }
    Ok(value)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    let parent = path
        .parent()
        .ok_or_else(|| AgentError::Mcp(format!("{} has no parent directory", path.display())))?;
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        MCP_FILE,
        std::process::id(),
        serial
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(AgentError::Mcp(format!(
            "cannot write {}: {error}",
            temp.display()
        )));
    }

    if let Err(error) = replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(AgentError::Mcp(format!(
            "cannot replace {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: both pointers point at NUL-terminated UTF-16 strings that live
    // for the duration of the call.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::scratch_dir;

    #[test]
    fn writes_the_direct_rustboard_entry() {
        let dir = scratch_dir("mcp-shape");
        let db = dir.join("board.sqlite3");
        let path = ensure_mcp_config(&dir, &db).expect("mcp config writes");

        assert_eq!(path, dir.join(".pi").join("mcp.json"));
        let value: Value = serde_json::from_slice(&fs::read(&path).expect("config reads"))
            .expect("config is JSON");
        let server = &value["mcpServers"]["rustboard"];
        assert_eq!(server["exposure"], "direct");
        assert_eq!(server["args"], json!(["--db", db.to_str().expect("utf8")]));
        let command = server["command"].as_str().expect("command is a string");
        assert!(
            command.ends_with(if cfg!(windows) {
                "rustboard-mcp.exe"
            } else {
                "rustboard-mcp"
            }),
            "command points at the sibling binary: {command}"
        );
        assert!(
            !server
                .as_object()
                .expect("server is object")
                .contains_key("env"),
            "no secrets or environment overrides"
        );
    }

    #[test]
    fn idempotent_and_preserves_unrelated_config() {
        let dir = scratch_dir("mcp-idempotent");
        let pi = dir.join(".pi");
        fs::create_dir_all(&pi).expect("pi dir");
        let path = pi.join("mcp.json");
        fs::write(
            &path,
            r#"{"theme":"dark","mcpServers":{"other":{"command":"other-mcp","args":[]}}}"#,
        )
        .expect("existing config writes");

        let db = dir.join("board.sqlite3");
        ensure_mcp_config(&dir, &db).expect("first write");
        let first = fs::read(&path).expect("first read");
        let value: Value = serde_json::from_slice(&first).expect("valid JSON");
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcpServers"]["other"]["command"], "other-mcp");
        assert_eq!(value["mcpServers"]["rustboard"]["exposure"], "direct");

        ensure_mcp_config(&dir, &db).expect("second write");
        let second = fs::read(&path).expect("second read");
        assert_eq!(
            first, second,
            "a second call leaves the file byte-identical"
        );
    }

    #[test]
    fn invalid_existing_config_is_reported_and_not_clobbered() {
        let dir = scratch_dir("mcp-invalid");
        let pi = dir.join(".pi");
        fs::create_dir_all(&pi).expect("pi dir");
        let path = pi.join("mcp.json");
        fs::write(&path, "this is not json").expect("existing config writes");

        let error = ensure_mcp_config(&dir, &dir.join("board.sqlite3"))
            .expect_err("invalid config is an error");
        assert!(matches!(error, AgentError::Mcp(_)));
        assert_eq!(
            fs::read_to_string(&path).expect("file still reads"),
            "this is not json"
        );
    }

    #[test]
    fn empty_file_is_treated_as_an_empty_config() {
        let dir = scratch_dir("mcp-empty");
        let pi = dir.join(".pi");
        fs::create_dir_all(&pi).expect("pi dir");
        fs::write(pi.join("mcp.json"), "  \n").expect("empty config writes");
        ensure_mcp_config(&dir, &dir.join("board.sqlite3")).expect("empty config is filled");
    }
}
