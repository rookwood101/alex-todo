//! Durable storage for the board and camera.
//!
//! eframe's built-in persistence silently disables itself when its app-data
//! folder cannot be created (it only writes a log line, and this app installs
//! no logger). When that happens the board is kept in memory only and the
//! next launch looks as if nothing was ever saved. This module owns the state
//! file instead: it resolves a verified location, writes atomically, and
//! surfaces failures so the UI can report them.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// eframe app id, which also names its preferred data folder.
const APP_ID: &str = "rustboard";

/// Name of the state file inside the chosen folder.
const STATE_FILE_NAME: &str = "board.ron";

/// Overrides the state-file location; mainly useful for tests and for pinning
/// a board to a specific folder.
pub const STATE_PATH_ENV: &str = "RUSTBOARD_STATE_PATH";

/// Read/write handle for the board state file.
#[derive(Clone, Debug)]
pub struct StateStore {
    /// Every location that may hold state, in priority order.
    candidates: Vec<PathBuf>,
    /// Where new saves are written.
    path: PathBuf,
    /// Why the preferred location cannot be used, until a save succeeds.
    last_error: Option<String>,
}

impl StateStore {
    /// Resolve the state file, preferring a writable location.
    pub fn open() -> Self {
        let candidates = candidate_paths();

        let writable = candidates.iter().find(|path| probe(path).is_ok()).cloned();
        let path = writable
            .or_else(|| candidates.iter().find(|path| path.is_file()).cloned())
            .or_else(|| candidates.first().cloned())
            .unwrap_or_else(|| PathBuf::from(STATE_FILE_NAME));

        let mut store = Self {
            candidates,
            path,
            last_error: None,
        };

        if let Err(err) = probe(&store.path) {
            store.last_error = Some(format!("No writable save location: {err}"));
        }
        store
    }

    /// Location new saves are written to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why saving is not currently possible, if it is not.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Read the newest readable state file, if any.
    ///
    /// A missing file is not an error; a malformed one is reported only when
    /// no other location holds a readable board.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        let mut problems = Vec::new();
        for path in &self.candidates {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    problems.push(format!("{}: {err}", path.display()));
                    continue;
                }
            };
            match ron::from_str(&text) {
                Ok(value) => return Ok(Some(value)),
                Err(err) => problems.push(format!("{}: {err}", path.display())),
            }
        }
        if problems.is_empty() {
            Ok(None)
        } else {
            Err(problems.join(" | "))
        }
    }

    /// Serialize `value` and replace the state file, falling back to the next
    /// candidate location if the current one stops accepting writes.
    pub fn save<T: Serialize>(&mut self, value: &T) -> Result<(), String> {
        let text = ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default())
            .map_err(|err| format!("Could not encode the board: {err}"))?;

        let mut targets = Vec::with_capacity(self.candidates.len());
        targets.push(self.path.clone());
        targets.extend(
            self.candidates
                .iter()
                .filter(|path| **path != self.path)
                .cloned(),
        );

        let mut problems = Vec::new();
        for target in targets {
            match write_atomic(&target, &text) {
                Ok(()) => {
                    self.path = target;
                    self.last_error = None;
                    return Ok(());
                }
                Err(err) => problems.push(format!("{}: {err}", target.display())),
            }
        }

        let message = problems.join(" | ");
        self.last_error = Some(message.clone());
        Err(message)
    }

    /// Store that only knows about one path, for tests.
    #[cfg(test)]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            candidates: vec![path.clone()],
            path,
            last_error: None,
        }
    }

    /// Store with a fixed candidate order, for tests.
    #[cfg(test)]
    pub fn with_candidates(candidates: Vec<PathBuf>) -> Self {
        let path = candidates
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from(STATE_FILE_NAME));
        Self {
            candidates,
            path,
            last_error: None,
        }
    }
}

/// Candidate state files, most preferred first.
fn candidate_paths() -> Vec<PathBuf> {
    if let Some(custom) = std::env::var_os(STATE_PATH_ENV).filter(|value| !value.is_empty()) {
        return vec![PathBuf::from(custom)];
    }

    let mut paths = Vec::new();
    if let Some(dir) = eframe::storage_dir(APP_ID) {
        paths.push(dir.join(STATE_FILE_NAME));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        paths.push(dir.join(STATE_FILE_NAME));
    }
    if let Ok(dir) = std::env::current_dir() {
        paths.push(dir.join(STATE_FILE_NAME));
    }
    paths.dedup();
    paths
}

/// Check that `path` can be created and written, creating its folder if needed.
fn probe(path: &Path) -> Result<(), String> {
    let dir = parent_dir(path).ok_or_else(|| format!("{} has no folder", path.display()))?;
    fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;

    let scratch = dir.join(format!(".rustboard-write-{}", std::process::id()));
    fs::write(&scratch, b"ok")
        .and_then(|()| fs::remove_file(&scratch))
        .map_err(|err| format!("{}: {err}", dir.display()))
}

/// Replace `path` with `contents` without ever exposing a half-written file.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = parent_dir(path) {
        fs::create_dir_all(dir)?;
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STATE_FILE_NAME);
    let scratch = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut file = File::create(&scratch)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    };

    if let Err(err) = write() {
        let _ = fs::remove_file(&scratch);
        return Err(err);
    }

    // `fs::rename` replaces the destination on Windows and Unix alike.
    fs::rename(&scratch, path).inspect_err(|_| {
        let _ = fs::remove_file(&scratch);
    })
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|dir| !dir.as_os_str().is_empty())
}

/// Scratch folder for tests.
///
/// Prefers the system temp folder, but falls back to the workspace `target`
/// folder: sandboxed build environments sometimes deny temp writes to child
/// processes even though normal desktop runs are unaffected.
#[cfg(test)]
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

    let serial = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    let unique = format!("rustboard-{name}-{}-{serial}", std::process::id());
    let candidates = [
        std::env::temp_dir().join(&unique),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(&unique),
    ];

    for dir in candidates {
        let _ = fs::remove_dir_all(&dir);
        if fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    panic!("no writable scratch folder for tests");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        text: String,
        zoom: f32,
    }

    fn sample() -> Sample {
        Sample {
            text: "hello board".to_owned(),
            zoom: 1.5,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        scratch_dir(name)
    }

    #[test]
    fn saves_and_loads_a_round_trip() {
        let path = temp_dir("round-trip").join(STATE_FILE_NAME);
        let mut store = StateStore::at(&path);

        assert_eq!(store.load::<Sample>().expect("empty store loads"), None);
        store.save(&sample()).expect("save succeeds");

        assert_eq!(store.path(), path);
        assert_eq!(
            store.load::<Sample>().expect("load succeeds"),
            Some(sample())
        );
    }

    #[test]
    fn save_replaces_the_file_without_leaving_scratch_files() {
        let dir = temp_dir("atomic");
        let path = dir.join(STATE_FILE_NAME);
        let mut store = StateStore::at(&path);

        store.save(&sample()).expect("first save");
        store
            .save(&Sample {
                text: "second".to_owned(),
                zoom: 2.0,
            })
            .expect("second save");

        let entries = fs::read_dir(&dir)
            .expect("read state folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from(STATE_FILE_NAME)]);

        let saved: Sample = store.load().expect("load succeeds").expect("state exists");
        assert_eq!(saved.text, "second");
    }

    #[test]
    fn malformed_state_is_reported_instead_of_crashing() {
        let path = temp_dir("malformed").join(STATE_FILE_NAME);
        fs::create_dir_all(path.parent().expect("temp dir")).expect("create temp dir");
        fs::write(&path, "this is not ron").expect("write garbage");

        let store = StateStore::at(&path);
        let error = store.load::<Sample>().expect_err("garbage is an error");
        assert!(
            error.contains("board.ron"),
            "error should name the file: {error}"
        );
    }

    #[test]
    fn unwritable_location_falls_back_to_the_next_candidate() {
        let dir = temp_dir("fallback");
        fs::create_dir_all(&dir).expect("create temp dir");
        // A file where a folder is expected makes the first location unusable.
        let blocker = dir.join("blocked");
        fs::write(&blocker, b"not a folder").expect("write blocker");

        let primary = blocker.join(STATE_FILE_NAME);
        let fallback = dir.join("fallback").join(STATE_FILE_NAME);
        let mut store = StateStore::with_candidates(vec![primary.clone(), fallback.clone()]);

        store.save(&sample()).expect("save via fallback");

        assert_eq!(store.path(), fallback);
        assert!(fallback.is_file());
        assert!(!primary.exists());
        assert_eq!(store.load::<Sample>().expect("load"), Some(sample()));
    }

    #[test]
    fn load_prefers_the_first_readable_candidate() {
        let dir = temp_dir("priority");
        let preferred = dir.join("preferred").join(STATE_FILE_NAME);
        let secondary = dir.join("secondary").join(STATE_FILE_NAME);

        let mut secondary_store = StateStore::at(&secondary);
        secondary_store
            .save(&Sample {
                text: "secondary".to_owned(),
                zoom: 1.0,
            })
            .expect("save secondary");

        let mut preferred_store = StateStore::at(&preferred);
        preferred_store.save(&sample()).expect("save preferred");

        let store = StateStore::with_candidates(vec![preferred, secondary]);
        let loaded: Sample = store.load().expect("load").expect("state");
        assert_eq!(loaded.text, "hello board");
    }

    #[test]
    fn missing_state_is_not_an_error() {
        let store = StateStore::at(temp_dir("missing").join(STATE_FILE_NAME));
        assert_eq!(store.load::<Sample>().expect("missing file is fine"), None);
    }
}
