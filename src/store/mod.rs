//! SQLite storage: connection setup, paths, migrations and integrity helpers.
//!
//! The board is a WAL database with a versioned migration history. This module
//! owns only the plumbing; the schema and its migration fixtures live in
//! crate::store::schema (added by task 030) and commands live in
//! crate::command (task 040).

pub mod paths;
pub mod schema;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::backup::Progress;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use sha2::{Digest, Sha256};

pub use paths::StorePaths;

/// Busy timeout for every connection, in milliseconds.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Errors from opening, migrating or checking the database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot determine the board home: set RUSTBOARD_HOME or LOCALAPPDATA")]
    MissingHome,

    #[error("cannot create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot read migrations from {path}: {source}")]
    ReadMigrations {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("migration file name {name:?} is not NNNN_description.sql")]
    BadMigrationName { name: String },

    #[error("duplicate migration version {version} ({first} and {second})")]
    DuplicateMigrationVersion {
        version: i64,
        first: String,
        second: String,
    },

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("cannot open database {path}: {source}")]
    OpenDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("database is not in WAL journal mode (got {mode:?})")]
    NotWal { mode: String },

    #[error("migration {version} failed: {source}")]
    MigrationFailed {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "migration {version} checksum mismatch: database has {found:?}, build expects {expected:?}"
    )]
    ChecksumMismatch {
        version: i64,
        expected: String,
        found: String,
    },

    #[error("database schema version {found} is newer than this build supports ({supported})")]
    DatabaseTooNew { found: i64, supported: i64 },

    #[error("applied migration {version} is not present in this build's migrations")]
    AppliedMigrationMissing { version: i64 },

    #[error("integrity check failed: {detail}")]
    Integrity { detail: String },

    #[error("cannot write pre-migration backup {path}: {source}")]
    Backup {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

/// One versioned SQL migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    pub version: i64,
    pub name: String,
    pub sql: String,
}

impl Migration {
    /// Lowercase SHA-256 of the exact SQL text, used to detect edited
    /// migrations after they have been applied.
    pub fn checksum(&self) -> String {
        let digest = Sha256::digest(self.sql.as_bytes());
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

/// An ordered, validated set of migrations.
#[derive(Clone, Debug, Default)]
pub struct Migrations {
    migrations: Vec<Migration>,
}

impl Migrations {
    /// Build from already-read migrations, sorting by version and rejecting
    /// duplicates or non-positive versions.
    pub fn new(mut migrations: Vec<Migration>) -> Result<Self, StoreError> {
        for migration in &migrations {
            if migration.version <= 0 {
                return Err(StoreError::BadMigrationName {
                    name: migration.name.clone(),
                });
            }
        }
        migrations.sort_by_key(|migration| migration.version);
        for pair in migrations.windows(2) {
            if pair[0].version == pair[1].version {
                return Err(StoreError::DuplicateMigrationVersion {
                    version: pair[0].version,
                    first: pair[0].name.clone(),
                    second: pair[1].name.clone(),
                });
            }
        }
        Ok(Self { migrations })
    }

    /// Read NNNN_name.sql files from a directory, in version order.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref();
        let entries = fs::read_dir(dir).map_err(|source| StoreError::ReadMigrations {
            path: dir.to_path_buf(),
            source,
        })?;

        let mut migrations = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::ReadMigrations {
                path: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("sql") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            let (version, name) = parse_migration_name(&file_name)?;
            let sql = fs::read_to_string(&path).map_err(|source| StoreError::ReadMigrations {
                path: path.clone(),
                source,
            })?;
            migrations.push(Migration { version, name, sql });
        }
        Self::new(migrations)
    }

    /// Build from embedded (file_name, sql) pairs (used by task 030).
    pub fn from_sql(entries: &[(&str, &str)]) -> Result<Self, StoreError> {
        let mut migrations = Vec::with_capacity(entries.len());
        for (file_name, sql) in entries {
            let (version, name) = parse_migration_name(file_name)?;
            migrations.push(Migration {
                version,
                name,
                sql: (*sql).to_owned(),
            });
        }
        Self::new(migrations)
    }

    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }

    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    /// Highest known version, or 0 for an empty set.
    pub fn latest_version(&self) -> i64 {
        self.migrations
            .last()
            .map(|migration| migration.version)
            .unwrap_or(0)
    }

    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    fn get(&self, version: i64) -> Option<&Migration> {
        self.migrations
            .binary_search_by_key(&version, |migration| migration.version)
            .ok()
            .map(|index| &self.migrations[index])
    }
}

/// NNNN_name.sql to (version, name). The prefix must be 1-6 digits and the
/// description must not be empty.
fn parse_migration_name(file_name: &str) -> Result<(i64, String), StoreError> {
    let bad = || StoreError::BadMigrationName {
        name: file_name.to_owned(),
    };
    let stem = file_name.strip_suffix(".sql").ok_or_else(bad)?;
    let (prefix, name) = stem.split_once('_').ok_or_else(bad)?;
    if prefix.is_empty() || prefix.len() > 6 || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad());
    }
    let version: i64 = prefix.parse().map_err(|_| bad())?;
    let name = name.trim();
    if version <= 0 || name.is_empty() {
        return Err(bad());
    }
    Ok((version, name.to_owned()))
}

/// A migration recorded as applied to this database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub applied_at_utc_ms: i64,
    pub checksum: String,
}

/// Outcome of running the migration list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Versions applied by this call.
    pub applied: Vec<i64>,
    /// Versions that were already present.
    pub already_applied: usize,
    /// Backup written before applying, if any.
    pub backup: Option<PathBuf>,
}

/// Handle to one open board database.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database at path and configure it for the
    /// board. Parent directories are created. Migrations are not run here; see
    /// Store::migrate and Store::open_board.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(&path).map_err(|source| StoreError::OpenDatabase {
            path: path.clone(),
            source,
        })?;
        configure_writer(&conn)?;
        Ok(Self { conn, path })
    }

    /// Open an existing database read-only. No migration is run and no folder
    /// is created; writes fail with SQLITE_READONLY.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
            |source| StoreError::OpenDatabase {
                path: path.clone(),
                source,
            },
        )?;
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn, path })
    }

    /// Open a board home, run the given migrations against its database and
    /// return the ready store plus a report.
    pub fn open_board(
        paths: &StorePaths,
        migrations: &Migrations,
    ) -> Result<(Self, MigrationReport), StoreError> {
        paths.ensure_dirs()?;
        let mut store = Self::open(paths.db())?;
        let report = store.migrate(migrations, paths.backups())?;
        Ok((store, report))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// PRAGMA data_version: changes when another connection commits.
    pub fn data_version(&self) -> Result<i64, StoreError> {
        let version = self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?;
        Ok(version)
    }

    /// Run PRAGMA integrity_check; every returned row must be "ok".
    pub fn check_integrity(&self) -> Result<(), StoreError> {
        let mut statement = self.conn.prepare("PRAGMA integrity_check")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut problems = Vec::new();
        for row in rows {
            let row = row?;
            if !row.eq_ignore_ascii_case("ok") {
                problems.push(row);
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Integrity {
                detail: problems.join("; "),
            })
        }
    }

    /// Apply every migration newer than the recorded history.
    ///
    /// Already-applied migrations are verified by checksum. A database whose
    /// recorded version is newer than the build is refused. When there are
    /// pending migrations, the database is first copied to
    /// backups/pre-migration-&lt;utc ms&gt;.sqlite3 using SQLite's backup API.
    pub fn migrate(
        &mut self,
        migrations: &Migrations,
        backups_dir: &Path,
    ) -> Result<MigrationReport, StoreError> {
        self.ensure_history_table()?;
        let applied = self.applied_migrations()?;
        let supported = migrations.latest_version();

        if let Some(latest) = applied.last()
            && latest.version > supported
        {
            return Err(StoreError::DatabaseTooNew {
                found: latest.version,
                supported,
            });
        }

        for record in &applied {
            let known =
                migrations
                    .get(record.version)
                    .ok_or(StoreError::AppliedMigrationMissing {
                        version: record.version,
                    })?;
            let expected = known.checksum();
            if !expected.eq_ignore_ascii_case(&record.checksum) {
                return Err(StoreError::ChecksumMismatch {
                    version: record.version,
                    expected,
                    found: record.checksum.clone(),
                });
            }
        }

        let highest = applied.last().map(|record| record.version).unwrap_or(0);
        let pending: Vec<&Migration> = migrations
            .migrations()
            .iter()
            .filter(|migration| migration.version > highest)
            .collect();

        let mut report = MigrationReport {
            already_applied: applied.len(),
            ..MigrationReport::default()
        };
        if pending.is_empty() {
            return Ok(report);
        }

        if !applied.is_empty() {
            report.backup = Some(self.backup_before_migration(backups_dir)?);
        }

        for migration in pending {
            self.apply_one(migration)?;
            report.applied.push(migration.version);
        }
        Ok(report)
    }

    fn ensure_history_table(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version            INTEGER PRIMARY KEY,
                 applied_at_utc_ms  INTEGER NOT NULL,
                 checksum           TEXT NOT NULL
             ) STRICT;",
        )?;
        Ok(())
    }

    fn applied_migrations(&self) -> Result<Vec<AppliedMigration>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT version, applied_at_utc_ms, checksum
             FROM schema_migrations
             ORDER BY version",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(AppliedMigration {
                version: row.get(0)?,
                applied_at_utc_ms: row.get(1)?,
                checksum: row.get(2)?,
            })
        })?;
        let mut applied = Vec::new();
        for row in rows {
            applied.push(row?);
        }
        Ok(applied)
    }

    fn backup_before_migration(&self, backups_dir: &Path) -> Result<PathBuf, StoreError> {
        fs::create_dir_all(backups_dir).map_err(|source| StoreError::CreateDir {
            path: backups_dir.to_path_buf(),
            source,
        })?;

        let base = format!("pre-migration-{}", now_utc_ms());
        let mut candidate = backups_dir.join(format!("{base}.sqlite3"));
        let mut suffix = 1;
        while candidate.exists() {
            candidate = backups_dir.join(format!("{base}-{suffix}.sqlite3"));
            suffix += 1;
        }

        self.conn
            .backup(rusqlite::MAIN_DB, &candidate, None::<fn(Progress)>)
            .map_err(|source| StoreError::Backup {
                path: candidate.clone(),
                source,
            })?;
        Ok(candidate)
    }

    fn apply_one(&mut self, migration: &Migration) -> Result<(), StoreError> {
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::MigrationFailed {
                version: migration.version,
                source,
            })?;

        transaction
            .execute_batch(&migration.sql)
            .map_err(|source| StoreError::MigrationFailed {
                version: migration.version,
                source,
            })?;
        transaction
            .execute(
                "INSERT INTO schema_migrations (version, applied_at_utc_ms, checksum)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![migration.version, now_utc_ms(), migration.checksum()],
            )
            .map_err(|source| StoreError::MigrationFailed {
                version: migration.version,
                source,
            })?;
        transaction
            .commit()
            .map_err(|source| StoreError::MigrationFailed {
                version: migration.version,
                source,
            })
    }
}

/// Apply the pragmas the board relies on, then confirm WAL actually took.
fn configure_writer(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::NotWal { mode });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(())
}

/// Milliseconds since the Unix epoch, saturating instead of panicking.
pub fn now_utc_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Scratch folder for store tests.
///
/// Prefers %TEMP% but falls back to the workspace target folder, mirroring the
/// pattern the old persistence tests use.
#[cfg(test)]
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

    let serial = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    let unique = format!("rustboard-store-{name}-{}-{serial}", std::process::id());
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
    panic!("no writable scratch folder for store tests");
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATION_1: &str = "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);";
    const MIGRATION_2: &str = "ALTER TABLE widgets ADD COLUMN colour TEXT;";

    fn migrations(entries: &[(&str, &str)]) -> Migrations {
        Migrations::from_sql(entries).expect("fixture migrations parse")
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let root = scratch_dir(name);
        let db = root.join("board.sqlite3");
        let backups = root.join("backups");
        (db, backups)
    }

    fn seed_widget(store: &Store, name: &str) -> rusqlite::Result<()> {
        store
            .connection()
            .execute("INSERT INTO widgets (name) VALUES (?1)", [name])?;
        Ok(())
    }

    fn widget_count(store: &Store) -> i64 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM widgets", [], |row| row.get(0))
            .expect("count widgets")
    }

    #[test]
    fn fresh_database_applies_every_migration() {
        let (db, backups) = fixture("fresh");
        let mut store = Store::open(&db).expect("open");
        let report = store
            .migrate(
                &migrations(&[
                    ("0001_widgets.sql", MIGRATION_1),
                    ("0002_colour.sql", MIGRATION_2),
                ]),
                &backups,
            )
            .expect("migrations apply");

        assert_eq!(report.applied, vec![1, 2]);
        assert_eq!(report.already_applied, 0);
        assert!(report.backup.is_none(), "nothing to back up on a fresh db");
        seed_widget(&store, "one").expect("insert");
        assert_eq!(widget_count(&store), 1);

        let applied = store.applied_migrations().expect("read history");
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].version, 1);
        assert_eq!(
            applied[0].checksum,
            migrations(&[("0001_widgets.sql", MIGRATION_1)]).migrations()[0].checksum()
        );
    }

    #[test]
    fn second_open_is_a_no_op() {
        let (db, backups) = fixture("noop");
        let set = migrations(&[("0001_widgets.sql", MIGRATION_1)]);

        let mut store = Store::open(&db).expect("open");
        store.migrate(&set, &backups).expect("first migrate");
        drop(store);

        let mut store = Store::open(&db).expect("reopen");
        let report = store.migrate(&set, &backups).expect("second migrate");
        assert!(report.applied.is_empty());
        assert_eq!(report.already_applied, 1);
        assert!(report.backup.is_none());
    }

    #[test]
    fn an_edited_migration_is_rejected() {
        let (db, backups) = fixture("checksum");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("first migrate");

        let edited = migrations(&[(
            "0001_widgets.sql",
            "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL, extra TEXT);",
        )]);
        let error = store
            .migrate(&edited, &backups)
            .expect_err("checksum must differ");
        assert!(matches!(
            error,
            StoreError::ChecksumMismatch { version: 1, .. }
        ));
    }

    #[test]
    fn a_newer_database_is_refused() {
        let (db, backups) = fixture("too-new");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(
                &migrations(&[
                    ("0001_widgets.sql", MIGRATION_1),
                    ("0002_colour.sql", MIGRATION_2),
                ]),
                &backups,
            )
            .expect("migrate to v2");
        drop(store);

        let mut store = Store::open(&db).expect("reopen");
        let error = store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect_err("newer db is refused");
        assert!(matches!(
            error,
            StoreError::DatabaseTooNew {
                found: 2,
                supported: 1
            }
        ));
    }

    #[test]
    fn upgrading_backs_up_the_database_first() {
        let (db, backups) = fixture("backup");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("migrate to v1");
        seed_widget(&store, "kept").expect("insert");
        drop(store);

        let mut store = Store::open(&db).expect("reopen");
        let report = store
            .migrate(
                &migrations(&[
                    ("0001_widgets.sql", MIGRATION_1),
                    ("0002_colour.sql", MIGRATION_2),
                ]),
                &backups,
            )
            .expect("upgrade to v2");

        let backup = report.backup.expect("a backup is written");
        assert!(backup.is_file(), "backup file exists: {}", backup.display());
        let backup_store = Store::open_read_only(&backup).expect("backup opens");
        assert_eq!(widget_count(&backup_store), 1, "backup holds the data");
    }

    #[test]
    fn a_failed_migration_rolls_back_its_version() {
        let (db, backups) = fixture("rollback");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("migrate to v1");

        let error = store
            .migrate(
                &migrations(&[
                    ("0001_widgets.sql", MIGRATION_1),
                    (
                        "0002_broken.sql",
                        "CREATE TABLE nope (id INTEGER); THIS IS NOT SQL;",
                    ),
                ]),
                &backups,
            )
            .expect_err("broken migration fails");
        assert!(matches!(
            error,
            StoreError::MigrationFailed { version: 2, .. }
        ));

        let applied = store.applied_migrations().expect("history readable");
        assert_eq!(applied.len(), 1, "failed version is not recorded");
        let tables: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='nope'",
                [],
                |row| row.get(0),
            )
            .expect("count tables");
        assert_eq!(tables, 0, "failed migration left nothing behind");
    }

    #[test]
    fn data_version_changes_when_another_connection_commits() {
        let (db, backups) = fixture("data-version");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("migrate");
        let before = store.data_version().expect("data version");

        let other = Store::open(&db).expect("second connection");
        other
            .connection()
            .execute("INSERT INTO widgets (name) VALUES ('other')", [])
            .expect("commit elsewhere");

        let after = store.data_version().expect("data version");
        assert_ne!(before, after, "data_version must notice the other commit");

        let visible: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM widgets", [], |row| row.get(0))
            .expect("count");
        assert_eq!(visible, 1);
    }

    #[test]
    fn a_read_only_connection_cannot_write() {
        let (db, backups) = fixture("read-only");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("migrate");
        seed_widget(&store, "one").expect("insert");

        let reader = Store::open_read_only(&db).expect("read-only open");
        assert_eq!(widget_count(&reader), 1);
        let error = reader
            .connection()
            .execute("INSERT INTO widgets (name) VALUES ('nope')", [])
            .expect_err("writes are rejected");
        assert!(
            matches!(
                error,
                rusqlite::Error::SqliteFailure(ref failure, _)
                    if failure.code == rusqlite::ErrorCode::ReadOnly
            ),
            "expected SQLITE_READONLY, got {error:?}"
        );
        drop(reader);
        drop(store);
    }

    #[test]
    fn pragmas_are_what_the_board_expects() {
        let (db, backups) = fixture("pragmas");
        let mut store = Store::open(&db).expect("open");
        store
            .migrate(&migrations(&[("0001_widgets.sql", MIGRATION_1)]), &backups)
            .expect("migrate");

        let journal: String = store
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        let foreign_keys: i64 = store
            .connection()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign keys");
        let synchronous: i64 = store
            .connection()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous");
        let temp_store: i64 = store
            .connection()
            .query_row("PRAGMA temp_store", [], |row| row.get(0))
            .expect("temp store");

        assert_eq!(journal.to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 1, "NORMAL");
        assert_eq!(temp_store, 2, "MEMORY");
        assert!(store.data_version().expect("data version") >= 1);
        store.check_integrity().expect("fresh database is intact");
    }

    #[test]
    fn migrations_from_a_directory_are_ordered_and_validated() {
        let dir = scratch_dir("migrations-dir");
        fs::write(dir.join("0002_second.sql"), MIGRATION_2).expect("write 2");
        fs::write(dir.join("0001_first.sql"), MIGRATION_1).expect("write 1");
        fs::write(dir.join("notes.md"), "ignored").expect("write ignored");

        let set = Migrations::from_dir(&dir).expect("read folder");
        assert_eq!(set.latest_version(), 2);
        assert_eq!(set.migrations()[0].version, 1);
        assert_eq!(set.migrations()[0].name, "first");

        fs::write(dir.join("0003_bad name.sql"), "SELECT 1;").expect("write bad");
        let set = Migrations::from_dir(&dir).expect("names may contain spaces");
        assert_eq!(set.latest_version(), 3);

        fs::write(dir.join("0001_dupe.sql"), "SELECT 1;").expect("write duplicate");
        let error = Migrations::from_dir(&dir).expect_err("duplicate versions are refused");
        assert!(matches!(
            error,
            StoreError::DuplicateMigrationVersion { version: 1, .. }
        ));

        fs::write(dir.join("nope.sql"), "SELECT 1;").expect("write nameless");
        let error = Migrations::from_dir(&dir).expect_err("bad name is refused");
        assert!(matches!(error, StoreError::BadMigrationName { .. }));
    }

    #[test]
    fn open_board_honours_the_paths_layout() {
        let home = scratch_dir("open-board");
        let paths = StorePaths::from_home(&home);
        let set = migrations(&[("0001_widgets.sql", MIGRATION_1)]);

        let (store, report) = Store::open_board(&paths, &set).expect("open board");
        assert_eq!(report.applied, vec![1]);
        assert_eq!(store.path(), paths.db());
        assert!(paths.backups().is_dir());
        assert!(paths.logs().is_dir());
        drop(store);

        let (store, report) = Store::open_board(&paths, &set).expect("reopen board");
        assert!(report.applied.is_empty());
        assert!(report.backup.is_none());
        drop(store);
    }
}
