//! Filesystem locations for the board database, its backups and logs.
//!
//! The default home is %LOCALAPPDATA%\Rustboard\ (override with RUSTBOARD_HOME);
//! a database path can be overridden per binary (--db) for tests and for pinning
//! a board to a folder.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::StoreError;

/// Environment variable that relocates the whole board home.
pub const HOME_ENV: &str = "RUSTBOARD_HOME";
/// Environment variable that points at the Windows per-user local app data root.
pub const LOCAL_APP_DATA_ENV: &str = "LOCALAPPDATA";
/// Database file name inside the board home.
pub const DB_FILE_NAME: &str = "board.sqlite3";
/// Folder name under %LOCALAPPDATA%.
pub const APP_FOLDER: &str = "Rustboard";
/// Sub-folder that receives pre-migration backups.
pub const BACKUPS_FOLDER: &str = "backups";
/// Sub-folder reserved for logs.
pub const LOGS_FOLDER: &str = "logs";

/// Resolved filesystem layout for one board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePaths {
    home: PathBuf,
    db: PathBuf,
    backups: PathBuf,
    logs: PathBuf,
    migrations: Option<PathBuf>,
}

impl StorePaths {
    /// Resolve the layout from the process environment.
    ///
    /// The db_override corresponds to the --db flag; RUSTBOARD_HOME takes
    /// precedence over %LOCALAPPDATA%.
    pub fn resolve(db_override: Option<&Path>) -> Result<Self, StoreError> {
        Self::resolve_from(
            db_override,
            std::env::var_os(HOME_ENV),
            std::env::var_os(LOCAL_APP_DATA_ENV),
        )
    }

    /// Pure resolution seam so tests do not have to mutate process globals.
    pub fn resolve_from(
        db_override: Option<&Path>,
        home_override: Option<OsString>,
        local_app_data: Option<OsString>,
    ) -> Result<Self, StoreError> {
        // A --db override pins the whole board next to that file, so its
        // backups and logs travel with it.
        if let Some(db) = db_override {
            return Ok(Self::from_db(db));
        }

        let home = match home_override.filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => match local_app_data.filter(|value| !value.is_empty()) {
                Some(base) => PathBuf::from(base).join(APP_FOLDER),
                None => return Err(StoreError::MissingHome),
            },
        };

        let db = home.join(DB_FILE_NAME);
        Ok(Self::from_home_and_db(home, db))
    }

    /// Layout rooted at an explicit home folder.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let db = home.join(DB_FILE_NAME);
        Self::from_home_and_db(home, db)
    }

    /// Layout derived from an explicit database file.
    pub fn from_db(db: impl Into<PathBuf>) -> Self {
        let db = db.into();
        let home = db
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_home_and_db(home, db)
    }

    fn from_home_and_db(home: PathBuf, db: PathBuf) -> Self {
        Self {
            backups: home.join(BACKUPS_FOLDER),
            logs: home.join(LOGS_FOLDER),
            home,
            db,
            migrations: None,
        }
    }

    /// Point the migration runner at an on-disk migrations folder.
    pub fn with_migrations(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations = Some(dir.into());
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn db(&self) -> &Path {
        &self.db
    }

    pub fn backups(&self) -> &Path {
        &self.backups
    }

    pub fn logs(&self) -> &Path {
        &self.logs
    }

    pub fn migrations(&self) -> Option<&Path> {
        self.migrations.as_deref()
    }

    /// Create the home, database parent, backups and logs folders.
    pub fn ensure_dirs(&self) -> Result<(), StoreError> {
        for dir in [self.home.as_path(), self.backups(), self.logs()] {
            fs::create_dir_all(dir).map_err(|source| StoreError::CreateDir {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        if let Some(parent) = self
            .db
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::store::scratch_dir(name)
    }

    #[test]
    fn rustboard_home_override_wins() {
        let home = scratch("paths-home").join("board-home");
        let paths = StorePaths::resolve_from(
            None,
            Some(home.clone().into_os_string()),
            Some(OsString::from("C:\\ignored")),
        )
        .expect("home override resolves");

        assert_eq!(paths.home(), home.as_path());
        assert_eq!(paths.db(), home.join(DB_FILE_NAME).as_path());
        assert_eq!(paths.backups(), home.join(BACKUPS_FOLDER).as_path());
        assert_eq!(paths.logs(), home.join(LOGS_FOLDER).as_path());
    }

    #[test]
    fn local_app_data_is_the_default_root() {
        let base = scratch("paths-lad");
        let paths = StorePaths::resolve_from(None, None, Some(base.clone().into_os_string()))
            .expect("local app data resolves");
        assert_eq!(paths.home(), base.join(APP_FOLDER).as_path());
        assert_eq!(
            paths.db(),
            base.join(APP_FOLDER).join(DB_FILE_NAME).as_path()
        );
    }

    #[test]
    fn a_db_override_keeps_backups_next_to_the_file() {
        let dir = scratch("paths-db");
        let db = dir.join("elsewhere").join("custom.sqlite3");
        let paths =
            StorePaths::resolve_from(Some(&db), Some(OsString::from("C:\\board-home")), None)
                .expect("db override resolves");

        assert_eq!(paths.db(), db.as_path());
        assert_eq!(
            paths.backups(),
            dir.join("elsewhere").join(BACKUPS_FOLDER).as_path()
        );
    }

    #[test]
    fn a_missing_home_is_a_typed_error() {
        let error = StorePaths::resolve_from(None, None, None).expect_err("no home");
        assert!(matches!(error, StoreError::MissingHome));
    }

    #[test]
    fn ensure_dirs_creates_the_layout() {
        let home = scratch("paths-ensure");
        let paths = StorePaths::from_home(&home);
        paths.ensure_dirs().expect("directories are created");

        assert!(home.is_dir());
        assert!(paths.backups().is_dir());
        assert!(paths.logs().is_dir());
    }
}
