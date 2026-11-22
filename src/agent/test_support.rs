//! Scratch directories and fake-child helpers for agent tests.
//!
//! The fake child is a real `.bat` launched through `cmd.exe /d /s /c`, so the
//! harness exercises its actual spawn, stdin, stdout and Job Object paths.
//! Nothing here starts Pi or touches the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A unique writable directory under `target/test-tmp` (never %%TEMP%%: the
/// index notes that temp-file creation is restricted on this machine).
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(format!(
            "rustboard-agent-{name}-{}-{serial}",
            std::process::id()
        ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("agent scratch directory is writable");
    dir
}

/// Write a UTF-8 fixture file.
pub(crate) fn write_text(path: &Path, contents: &str) {
    fs::write(path, contents).expect("fixture writes");
}

/// Write JSONL, guaranteeing the final record is LF-terminated (strict RPC
/// framing: a reader must see LF before it can dispatch a record).
fn write_jsonl(path: &Path, contents: &str) {
    if contents.ends_with('\n') {
        write_text(path, contents);
    } else {
        write_text(path, &format!("{contents}\n"));
    }
}

/// Write a batch child that echoes `stem.jsonl` for every stdin line and
/// exits cleanly at EOF. Returns the batch path.
pub(crate) fn looping_child(dir: &Path, stem: &str, jsonl: &str) -> PathBuf {
    write_jsonl(&dir.join(format!("{stem}.jsonl")), jsonl);
    let bat = format!(
        "@echo off\r\n\
         setlocal EnableExtensions DisableDelayedExpansion\r\n\
         :rustboard_read\r\n\
         set \"line=\"\r\n\
         set /p \"line=\"\r\n\
         if not defined line goto rustboard_eof\r\n\
         type \"%~dp0{stem}.jsonl\"\r\n\
         goto rustboard_read\r\n\
         :rustboard_eof\r\n\
         exit /b 0\r\n"
    );
    let path = dir.join(format!("{stem}.bat"));
    write_text(&path, &bat);
    path
}

/// Write a batch child that answers exactly one prompt with `stem.jsonl` and
/// then exits, used to exercise clean-exit restart behaviour.
pub(crate) fn one_shot_child(dir: &Path, stem: &str, jsonl: &str) -> PathBuf {
    write_jsonl(&dir.join(format!("{stem}.jsonl")), jsonl);
    let bat = format!(
        "@echo off\r\n\
         setlocal EnableExtensions DisableDelayedExpansion\r\n\
         set \"line=\"\r\n\
         set /p \"line=\"\r\n\
         if not defined line goto rustboard_eof\r\n\
         type \"%~dp0{stem}.jsonl\"\r\n\
         :rustboard_eof\r\n\
         exit /b 0\r\n"
    );
    let path = dir.join(format!("{stem}.bat"));
    write_text(&path, &bat);
    path
}

/// A batch that streams a partial answer, waits for the abort line, then
/// emits the aborted `message_end` and `agent_settled` pair.
pub(crate) fn aborting_child(dir: &Path) -> PathBuf {
    write_jsonl(
        &dir.join("abort-stream.jsonl"),
        "{\"type\":\"agent_start\"}\n\
         {\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_start\",\"contentIndex\":0}}\n\
         {\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Working\"}}\n",
    );
    write_jsonl(
        &dir.join("abort-settled.jsonl"),
        "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"aborted\",\"errorMessage\":\"Request was aborted\"}}\n\
         {\"type\":\"agent_end\",\"willRetry\":false}\n\
         {\"type\":\"agent_settled\",\"aborted\":true}\n\
         {\"type\":\"response\",\"id\":\"unmatched\",\"command\":\"abort\",\"success\":true}\n",
    );
    let bat = "@echo off\r\n\
               setlocal EnableExtensions DisableDelayedExpansion\r\n\
               set \"line=\"\r\n\
               set /p \"line=\"\r\n\
               if not defined line goto rustboard_eof\r\n\
               type \"%~dp0abort-stream.jsonl\"\r\n\
               set \"line=\"\r\n\
               set /p \"line=\"\r\n\
               if not defined line goto rustboard_eof\r\n\
               type \"%~dp0abort-settled.jsonl\"\r\n\
               :rustboard_eof\r\n\
               exit /b 0\r\n";
    let path = dir.join("abort.bat");
    write_text(&path, bat);
    path
}

/// A batch that answers one prompt, starts a long-lived PowerShell
/// grandchild (which writes its PID), and then waits on stdin. Dropping the
/// harness must kill the grandchild through the Job Object.
pub(crate) fn tree_child(dir: &Path, jsonl: &str) -> PathBuf {
    write_jsonl(&dir.join("tree.jsonl"), jsonl);
    let bat = "@echo off\r\n\
               setlocal EnableExtensions DisableDelayedExpansion\r\n\
               set \"line=\"\r\n\
               set /p \"line=\"\r\n\
               if not defined line goto rustboard_eof\r\n\
               type \"%~dp0tree.jsonl\"\r\n\
               start \"\" /b powershell -NoProfile -NonInteractive -Command \"$PID | Set-Content -Encoding ascii '%~dp0grandchild.pid'; Start-Sleep -Seconds 30\"\r\n\
               :rustboard_read\r\n\
               set \"line=\"\r\n\
               set /p \"line=\"\r\n\
               if not defined line goto rustboard_eof\r\n\
               goto rustboard_read\r\n\
               :rustboard_eof\r\n\
               exit /b 0\r\n";
    let path = dir.join("tree.bat");
    write_text(&path, bat);
    path
}
