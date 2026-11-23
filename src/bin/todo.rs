//! todo - the debug CLI for the SQLite board (task 040).
//!
//! This is a thin shell over the library: it resolves the board, builds
//! Command values, applies them through one batch, and prints human or JSON
//! output. Exit codes are stable for scripts:
//!
//! * 0 - success
//! * 2 - usage error or rejected input
//! * 3 - note/edge not found
//! * 5 - database or internal error

use std::io::{Read, Write};
use std::path::PathBuf;

use rustboard::command::{Actor, Applied, Command, CommandError};
use rustboard::model::{Board, Note, Status};
use rustboard::store::{Store, StorePaths, schema};
use rustboard::time::{self, TimeSpec};

const USAGE: &str = "\
todo - Rustboard command-line board

Usage:
  todo [--db PATH] [--tz ZONE] [--json] COMMAND [ARGS]

Commands:
  add TEXT...            Add a note (capture syntax unless --raw)
      --in PATH          put the note in a container such as 'work/q4' (created if missing)
      --parent ID        make the note a child of another note
      --root             put the note at the root instead of the Inbox
      --due WHEN         deadline (civil time, date, or phrase like 'tomorrow 4pm')
      --remind WHEN      reminder time
      --important        mark the note important
      --raw              store TEXT exactly as typed
  list                   List active notes (--json for the whole board)
  show ID                Show one note
  set ID [--text T] [--due WHEN] [--remind WHEN]
         [--important yes|no] [--snooze WHEN] [--unsnooze]
  done ID                Mark a note done
  reopen ID              Reopen a note (re-arms its reminder)
  delete ID              Soft-delete a note and its subtree
  restore ID             Restore a deleted subtree
  snooze ID WHEN         Snooze a note
  move ID [--parent ID] [--root] [--before ID]
  link FROM TO [--kind K]
  apply --json           Read a JSON array of commands from stdin
      --agent            record the batch as the agent
  query --read-only SQL  Run one read-only statement
  path                   Print the database path
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut stdin = std::io::stdin();
    let code = run_with(args, &mut stdin, &mut stdout, &mut stderr);
    std::process::exit(code);
}

/// Tested entry point: no process exit, output goes to the given writers.
pub fn run_with(
    args: Vec<String>,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match dispatch(args, input, out, err) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            2
        }
    }
}

fn dispatch(
    args: Vec<String>,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let mut db: Option<PathBuf> = None;
    let mut json = false;
    let mut tz: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--db" => {
                index += 1;
                db = Some(PathBuf::from(args.get(index).ok_or("--db needs a path")?));
            }
            "--json" => json = true,
            "--tz" => {
                index += 1;
                tz = Some(args.get(index).ok_or("--tz needs a zone")?.clone());
            }
            "-h" | "--help" => {
                let _ = write!(out, "{USAGE}");
                return Ok(0);
            }
            other => rest.push(other.to_owned()),
        }
        index += 1;
    }

    let tz = tz.unwrap_or_else(time::system_zone);
    let paths = match StorePaths::resolve(db.as_deref()) {
        Ok(paths) => paths,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Ok(5);
        }
    };

    let Some(command) = rest.first().cloned() else {
        let _ = write!(err, "{USAGE}");
        return Ok(2);
    };
    let args = &rest[1..];

    if command == "path" {
        let _ = writeln!(out, "{}", paths.db().display());
        return Ok(0);
    }
    if command == "query" {
        return query(&paths, args, out, err);
    }

    let mut store = match schema::open_board(&paths) {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Ok(5);
        }
    };
    match command.as_str() {
        "add" => add(&mut store, args, json, &tz, out, err),
        "list" => list(&store, json, out),
        "show" => show(&store, args, json, out, err),
        "set" => set(&mut store, args, &tz, json, out, err),
        "done" | "complete" => simple(
            &mut store,
            args,
            |id| Command::Complete { id },
            json,
            out,
            err,
        ),
        "reopen" => simple(
            &mut store,
            args,
            |id| Command::Reopen { id },
            json,
            out,
            err,
        ),
        "delete" | "rm" => simple(
            &mut store,
            args,
            |id| Command::Delete { id },
            json,
            out,
            err,
        ),
        "restore" => simple(
            &mut store,
            args,
            |id| Command::Restore { id },
            json,
            out,
            err,
        ),
        "snooze" => snooze(&mut store, args, &tz, json, out, err),
        "move" => move_note(&mut store, args, json, out, err),
        "link" => link(&mut store, args, json, out, err),
        "apply" => apply(&mut store, args, input, json, out, err),
        other => {
            let _ = writeln!(err, "error: unknown command {other:?}\n\n{USAGE}");
            Ok(2)
        }
    }
}

fn print_applied(applied: &Applied, json: bool, out: &mut dyn Write) -> Result<(), String> {
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(applied).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    } else {
        writeln!(
            out,
            "batch {} applied {} command(s)",
            applied.batch_id,
            applied.effects.len()
        )
        .map_err(|error| error.to_string())?;
        for id in &applied.new_ids.notes {
            writeln!(out, "note {id}").map_err(|error| error.to_string())?;
        }
        for id in &applied.new_ids.edges {
            writeln!(out, "edge {id}").map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn apply_commands(
    store: &mut Store,
    commands: Vec<Command>,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match store.apply(&commands, Actor::Human, Some("todo cli")) {
        Ok(applied) => {
            if let Err(message) = print_applied(&applied, json, out) {
                let _ = writeln!(err, "error: {message}");
                return 5;
            }
            0
        }
        Err(error) => report_command_error(&error, err),
    }
}

fn report_command_error(error: &CommandError, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "error: {error}");
    match root_error(error) {
        CommandError::NotFound { .. } => 3,
        CommandError::Invalid { .. }
        | CommandError::Inbox { .. }
        | CommandError::Cycle
        | CommandError::Empty => 2,
        _ => 5,
    }
}

fn root_error(error: &CommandError) -> &CommandError {
    match error {
        CommandError::At { source, .. } => root_error(source),
        other => other,
    }
}

fn add(
    store: &mut Store,
    args: &[String],
    json: bool,
    tz: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut container: Option<String> = None;
    let mut parent: Option<String> = None;
    let mut at_root = false;
    let mut due: Option<TimeSpec> = None;
    let mut remind: Option<TimeSpec> = None;
    let mut important = false;
    let mut source = "human".to_owned();
    let mut raw = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--in" | "--group" => {
                index += 1;
                container = Some(
                    args.get(index)
                        .ok_or("--in needs a container path")?
                        .clone(),
                );
            }
            "--parent" => {
                index += 1;
                parent = Some(args.get(index).ok_or("--parent needs an id")?.clone());
            }
            "--root" => at_root = true,
            "--due" => {
                index += 1;
                due = Some(parse_when(
                    args.get(index).ok_or("--due needs a time")?,
                    tz,
                )?);
            }
            "--remind" => {
                index += 1;
                remind = Some(parse_when(
                    args.get(index).ok_or("--remind needs a time")?,
                    tz,
                )?);
            }
            "--important" => important = true,
            "--source" => {
                index += 1;
                source = args.get(index).ok_or("--source needs a value")?.clone();
            }
            "--raw" => raw = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?} for add"));
            }
            other => text_parts.push(other.to_owned()),
        }
        index += 1;
    }

    let typed = text_parts.join(" ");
    if typed.trim().is_empty() {
        let _ = writeln!(err, "error: add needs some text");
        return Ok(2);
    }

    let board = Board::load(store.connection()).map_err(|error| error.to_string())?;
    let known: Vec<String> = board
        .notes
        .iter()
        .filter(|note| note.parent.is_none() && board.has_children(&note.id))
        .map(|note| note.text.lines().next().unwrap_or("").trim().to_owned())
        .collect();
    let parsed =
        rustboard::capture::parse_capture(&typed, rustboard::store::now_utc_ms(), tz, &known);

    let command = Command::AddNote {
        text: if raw {
            typed.clone()
        } else {
            parsed.text.clone()
        },
        container: parsed
            .group
            .as_ref()
            .map(|group| group.name.clone())
            .or(container),
        parent,
        at_root,
        due: parsed.due.or(due),
        remind,
        important: parsed.important || important,
        source,
    };

    Ok(apply_commands(store, vec![command], json, out, err))
}

fn list(store: &Store, json: bool, out: &mut dyn Write) -> Result<i32, String> {
    let board = Board::load(store.connection()).map_err(|error| error.to_string())?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&board).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    } else if board.notes.is_empty() {
        writeln!(out, "no notes").map_err(|error| error.to_string())?;
    } else {
        for note in &board.notes {
            writeln!(out, "{}\t{}\t{}", note.id, status_label(note), note.text)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(0)
}

fn status_label(note: &Note) -> &'static str {
    match note.status {
        Status::Open => "open",
        Status::Done => "done",
    }
}

fn show(
    store: &Store,
    args: &[String],
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let id = args.first().ok_or("show needs a note id")?;
    let note =
        rustboard::model::load_note(store.connection(), id).map_err(|error| error.to_string())?;
    let Some(note) = note.filter(|note| note.deleted_at_utc_ms.is_none()) else {
        let _ = writeln!(err, "error: note {id:?} not found");
        return Ok(3);
    };
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&note).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    } else {
        writeln!(out, "id:       {}", note.id).map_err(|error| error.to_string())?;
        writeln!(out, "text:     {}", note.text).map_err(|error| error.to_string())?;
        writeln!(out, "status:   {}", status_label(&note)).map_err(|error| error.to_string())?;
        if let Some(parent) = &note.parent {
            writeln!(out, "parent:   {parent}").map_err(|error| error.to_string())?;
        }
        if note.important {
            writeln!(out, "important: yes").map_err(|error| error.to_string())?;
        }
        if note.inbox {
            writeln!(out, "role:     inbox").map_err(|error| error.to_string())?;
        }
        if let Some(due) = &note.due {
            writeln!(out, "due:      {} {}", due.local, due.tz)
                .map_err(|error| error.to_string())?;
        }
        if let Some(remind) = &note.remind {
            writeln!(out, "remind:   {} {}", remind.local, remind.tz)
                .map_err(|error| error.to_string())?;
        }
        writeln!(out, "version:  {}", note.version).map_err(|error| error.to_string())?;
    }
    Ok(0)
}

fn set(
    store: &mut Store,
    args: &[String],
    tz: &str,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let id = args.first().ok_or("set needs a note id")?.clone();
    let mut commands = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--text" => {
                index += 1;
                commands.push(Command::SetText {
                    id: id.clone(),
                    text: args.get(index).ok_or("--text needs a value")?.clone(),
                });
            }
            "--due" => {
                index += 1;
                commands.push(Command::SetDue {
                    id: id.clone(),
                    due: Some(parse_when(
                        args.get(index).ok_or("--due needs a time")?,
                        tz,
                    )?),
                });
            }
            "--remind" => {
                index += 1;
                commands.push(Command::SetRemind {
                    id: id.clone(),
                    remind: Some(parse_when(
                        args.get(index).ok_or("--remind needs a time")?,
                        tz,
                    )?),
                });
            }
            "--important" => {
                index += 1;
                commands.push(Command::SetImportant {
                    id: id.clone(),
                    important: parse_yes_no(args.get(index).ok_or("--important needs yes or no")?)?,
                });
            }
            "--snooze" => {
                index += 1;
                commands.push(Command::Snooze {
                    id: id.clone(),
                    until: parse_when(args.get(index).ok_or("--snooze needs a time")?, tz)?,
                });
            }
            "--unsnooze" => commands.push(Command::Unsnooze { id: id.clone() }),
            other => return Err(format!("unknown option {other:?} for set")),
        }
        index += 1;
    }
    if commands.is_empty() {
        let _ = writeln!(err, "error: set needs at least one field");
        return Ok(2);
    }
    Ok(apply_commands(store, commands, json, out, err))
}

fn simple(
    store: &mut Store,
    args: &[String],
    build: impl Fn(String) -> Command,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let id = args.first().ok_or("this command needs a note id")?.clone();
    Ok(apply_commands(store, vec![build(id)], json, out, err))
}

fn snooze(
    store: &mut Store,
    args: &[String],
    tz: &str,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let id = args.first().ok_or("snooze needs a note id")?.clone();
    let when = args.get(1).ok_or("snooze needs a time")?;
    let command = Command::Snooze {
        id,
        until: parse_when(when, tz)?,
    };
    Ok(apply_commands(store, vec![command], json, out, err))
}

fn move_note(
    store: &mut Store,
    args: &[String],
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let id = args.first().ok_or("move needs a note id")?.clone();
    let mut commands = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--parent" => {
                index += 1;
                commands.push(Command::SetParent {
                    id: id.clone(),
                    parent: Some(args.get(index).ok_or("--parent needs an id")?.clone()),
                    before: None,
                });
            }
            "--root" => {
                commands.push(Command::SetParent {
                    id: id.clone(),
                    parent: None,
                    before: None,
                });
            }
            "--before" => {
                index += 1;
                let before = args
                    .get(index)
                    .ok_or("--before needs a sibling id")?
                    .clone();
                match commands.last_mut() {
                    Some(Command::SetParent { before: slot, .. }) => *slot = Some(before),
                    _ => {
                        let board = Board::load(store.connection()).map_err(|e| e.to_string())?;
                        let parent = board
                            .note(&id)
                            .ok_or_else(|| format!("note {id:?} not found"))?
                            .parent
                            .clone();
                        commands.push(Command::SetParent {
                            id: id.clone(),
                            parent,
                            before: Some(before),
                        });
                    }
                }
            }
            other => return Err(format!("unknown option {other:?} for move")),
        }
        index += 1;
    }
    if commands.is_empty() {
        let _ = writeln!(err, "error: move needs --parent, --root or --before");
        return Ok(2);
    }
    Ok(apply_commands(store, commands, json, out, err))
}

fn link(
    store: &mut Store,
    args: &[String],
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let mut positional = Vec::new();
    let mut kind = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--kind" => {
                index += 1;
                kind = Some(args.get(index).ok_or("--kind needs a value")?.clone());
            }
            other => positional.push(other.to_owned()),
        }
        index += 1;
    }
    if positional.len() != 2 {
        let _ = writeln!(err, "error: link needs FROM and TO");
        return Ok(2);
    }
    let command = Command::Link {
        from: positional[0].clone(),
        to: positional[1].clone(),
        kind,
    };
    Ok(apply_commands(store, vec![command], json, out, err))
}

fn apply(
    store: &mut Store,
    args: &[String],
    input: &mut dyn Read,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    if !json && !args.iter().any(|arg| arg == "--json") {
        let _ = writeln!(err, "error: apply reads a JSON array; pass --json");
        return Ok(2);
    }
    let actor = if args.iter().any(|arg| arg == "--agent") {
        Actor::Agent
    } else {
        Actor::Human
    };
    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    let commands: Vec<Command> = match serde_json::from_str(&text) {
        Ok(commands) => commands,
        Err(error) => {
            let _ = writeln!(err, "error: invalid command JSON: {error}");
            return Ok(2);
        }
    };
    match store.apply_with(&commands, actor, None, Some("todo apply")) {
        Ok(applied) => {
            if let Err(message) = print_applied(&applied, json, out) {
                let _ = writeln!(err, "error: {message}");
                return Ok(5);
            }
            Ok(0)
        }
        Err(error) => Ok(report_command_error(&error, err)),
    }
}

fn query(
    paths: &StorePaths,
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    if !args.iter().any(|arg| arg == "--read-only") {
        let _ = writeln!(err, "error: query requires --read-only");
        return Ok(2);
    }
    let sql = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .ok_or("query needs a SQL statement")?;

    let store = match Store::open_read_only(paths.db()) {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Ok(5);
        }
    };
    let mut statement = match store.connection().prepare(sql) {
        Ok(statement) => statement,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Ok(5);
        }
    };

    let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
    let mut rows_json = Vec::new();
    let mut truncated = false;
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Ok(5);
        }
    };
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                return Ok(5);
            }
        };
        if rows_json.len() >= 500 {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            let value = match row.get_ref(index) {
                Ok(value) => value,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return Ok(5);
                }
            };
            values.push(value_to_json(value));
        }
        rows_json.push(values);
    }

    let payload = serde_json::json!({
        "columns": columns,
        "rows": rows_json,
        "truncated": truncated,
    });
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?
    )
    .map_err(|error| error.to_string())?;
    Ok(0)
}

fn value_to_json(value: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(number) => serde_json::Value::from(number),
        ValueRef::Real(number) => serde_json::Value::from(number),
        ValueRef::Text(bytes) => {
            serde_json::Value::from(String::from_utf8_lossy(bytes).into_owned())
        }
        ValueRef::Blob(bytes) => serde_json::Value::from(format!("<blob {} bytes>", bytes.len())),
    }
}

fn parse_yes_no(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "1" | "on" => Ok(true),
        "no" | "false" | "0" | "off" => Ok(false),
        other => Err(format!("expected yes or no, not {other:?}")),
    }
}

/// Accepts a civil timestamp, a bare date, or a capture phrase such as
/// "tomorrow 4pm" / "fri at noon".
fn parse_when(value: &str, tz: &str) -> Result<TimeSpec, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty time".to_owned());
    }

    let direct = if value.len() == 10 {
        Some(format!("{value}T09:00:00"))
    } else if value.contains('T') {
        Some(value.to_owned())
    } else {
        None
    };
    if let Some(local) = direct {
        let spec = TimeSpec {
            local,
            tz: tz.to_owned(),
        };
        spec.resolve().map_err(|error| error.to_string())?;
        return Ok(spec);
    }

    let parsed = rustboard::capture::parse_capture(
        &format!("x {value}"),
        rustboard::store::now_utc_ms(),
        tz,
        &[],
    );
    parsed
        .due
        .ok_or_else(|| format!("cannot understand the time {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::path::Path;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("rustboard-todo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn cli_in(dir: &Path, args: &[&str], input: &str) -> (i32, String, String) {
        let db = dir.join("board.sqlite3");
        let mut all: Vec<String> = vec!["--db".to_owned(), db.display().to_string()];
        all.extend(args.iter().map(|arg| (*arg).to_owned()));
        let mut stdin = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(all, &mut stdin, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    fn cli(name: &str, args: &[&str], input: &str) -> (i32, String, String) {
        let dir = scratch(name);
        cli_in(&dir, args, input)
    }

    fn board_json(dir: &Path) -> serde_json::Value {
        let (code, out, err) = cli_in(dir, &["list", "--json"], "");
        assert_eq!(code, 0, "list failed: {err}");
        serde_json::from_str(&out).expect("board json")
    }

    #[test]
    fn add_then_list_shows_the_note() {
        let dir = scratch("add");
        let (code, out, err) = cli_in(&dir, &["add", "buy", "milk"], "");
        assert_eq!(code, 0, "add failed: {err}");
        assert!(out.contains("note "), "human output names the note: {out}");

        let board = board_json(&dir);
        let notes = board["notes"].as_array().expect("notes array");
        assert_eq!(notes.len(), 2, "the note and the Inbox it landed in");
        let inbox = notes
            .iter()
            .find(|note| note["inbox"] == true)
            .expect("inbox");
        let note = notes
            .iter()
            .find(|note| note["text"] == "buy milk")
            .expect("note");
        assert_eq!(note["status"], "open");
        assert_eq!(note["parent"], inbox["id"]);
    }

    #[test]
    fn add_parses_capture_syntax() {
        let dir = scratch("capture");
        let (code, _out, err) = cli_in(&dir, &["add", "buy milk tomorrow #home"], "");
        assert_eq!(code, 0, "add failed: {err}");
        let board = board_json(&dir);
        let notes = board["notes"].as_array().expect("notes");
        let note = notes
            .iter()
            .find(|note| note["text"] == "buy milk")
            .expect("note");
        let home = notes
            .iter()
            .find(|candidate| candidate["text"] == "home")
            .expect("container");
        assert_eq!(note["parent"], home["id"]);
        assert!(note["due"]["local"].as_str().expect("due").contains('T'));
    }

    #[test]
    fn delete_hides_and_restore_returns() {
        let dir = scratch("delete");
        let (code, out, err) = cli_in(&dir, &["add", "--root", "temporary"], "");
        assert_eq!(code, 0, "add failed: {err}");
        let id = out
            .lines()
            .find_map(|line| line.strip_prefix("note "))
            .expect("note id")
            .to_owned();

        let (code, _out, err) = cli_in(&dir, &["delete", &id], "");
        assert_eq!(code, 0, "delete failed: {err}");
        assert_eq!(board_json(&dir)["notes"].as_array().unwrap().len(), 0);

        let (code, _out, err) = cli_in(&dir, &["restore", &id], "");
        assert_eq!(code, 0, "restore failed: {err}");
        assert_eq!(board_json(&dir)["notes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn apply_reads_a_json_batch() {
        let dir = scratch("apply");
        let input = r#"[{"command":"add_note","text":"from json","at_root":true}]"#;
        let (code, out, err) = cli_in(&dir, &["apply", "--json"], input);
        assert_eq!(code, 0, "apply failed: {err} {out}");
        let board = board_json(&dir);
        assert_eq!(board["notes"][0]["text"], "from json");
    }

    #[test]
    fn query_is_read_only_and_reports_columns() {
        let dir = scratch("query");
        let (code, _out, err) = cli_in(&dir, &["add", "--root", "one"], "");
        assert_eq!(code, 0, "add failed: {err}");

        let (code, out, err) = cli_in(
            &dir,
            &["query", "--read-only", "SELECT COUNT(*) AS n FROM notes"],
            "",
        );
        assert_eq!(code, 0, "query failed: {err}");
        let payload: serde_json::Value = serde_json::from_str(&out).expect("query json");
        assert_eq!(payload["columns"][0], "n");
        assert_eq!(payload["rows"][0][0], 1);

        let (code, _out, err) = cli_in(
            &dir,
            &[
                "query",
                "--read-only",
                "INSERT INTO notes (id, text, created_at_utc_ms, updated_at_utc_ms) VALUES ('x','y',1,1)",
            ],
            "",
        );
        assert_eq!(code, 5, "a write must fail: {err}");
        assert_eq!(
            board_json(&dir)["notes"].as_array().unwrap().len(),
            1,
            "the failed write changed nothing"
        );
    }

    #[test]
    fn missing_note_uses_exit_code_3() {
        let (code, _out, err) = cli("missing", &["done", "nope"], "");
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("not found"));
    }

    #[test]
    fn usage_errors_use_exit_code_2() {
        let (code, _out, _err) = cli("usage", &[], "");
        assert_eq!(code, 2);
        let (code, _out, _err) = cli("usage", &["frobnicate"], "");
        assert_eq!(code, 2);
    }

    #[test]
    fn path_prints_the_database_path() {
        let dir = scratch("path");
        let db = dir.join("board.sqlite3");
        let args = vec![
            "--db".to_owned(),
            db.display().to_string(),
            "path".to_owned(),
        ];
        let mut stdin = Cursor::new(Vec::new());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(args, &mut stdin, &mut out, &mut err);
        assert_eq!(code, 0);
        let printed = String::from_utf8_lossy(&out);
        assert!(printed.trim().ends_with("board.sqlite3"));
    }

    #[test]
    fn set_and_snooze_round_trip() {
        let dir = scratch("set");
        let (code, out, err) = cli_in(&dir, &["add", "task"], "");
        assert_eq!(code, 0, "{err}");
        let id = out
            .lines()
            .find_map(|line| line.strip_prefix("note "))
            .expect("id")
            .to_owned();
        let (code, _out, err) = cli_in(
            &dir,
            &[
                "set",
                &id,
                "--text",
                "renamed",
                "--important",
                "yes",
                "--due",
                "tomorrow",
            ],
            "",
        );
        assert_eq!(code, 0, "set failed: {err}");
        let board = board_json(&dir);
        let note = board["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .find(|note| note["id"] == id.as_str())
            .expect("note")
            .clone();
        assert_eq!(note["text"], "renamed");
        assert_eq!(note["important"], true);
        assert!(note["due"]["local"].as_str().is_some());
    }
}
