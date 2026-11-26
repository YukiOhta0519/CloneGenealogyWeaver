pub mod reconstruct;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, Transaction};
use rustix::fs::{FlockOperation, flock};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::version::{EVENTS_SCHEMA_VERSION, GENEALOGY_SCHEMA_VERSION};

/// 表の形の版だけで見分けないのは，どちらのファイルも版を 1 から数えるからである．
pub const EVENTS_APPLICATION_ID: i32 = i32::from_be_bytes(*b"CGWE");
pub const GENEALOGY_APPLICATION_ID: i32 = i32::from_be_bytes(*b"CGWG");

/// 待たずに失敗しないのは，pairs が読んでいる間に record が書くような場合に止まらないためである．
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const EVENTS_SQL: &str = include_str!("../../schema/events.sql");
const CLAIM_SQL: &str = include_str!("../../schema/plugins/claim.sql");
const FIXTURE_SQL: &str = include_str!("../../schema/plugins/fixture.sql");
const TEST_PATH_SQL: &str = include_str!("../../schema/plugins/test_path.sql");
const GENEALOGY_SQL: &str = include_str!("../../schema/genealogy.sql");

pub struct ClassifierTables {
    pub sql: &'static str,
    pub labels: bool,
    /// 1 列目が版の番号の表．再開で版の番号で同じ手順で消す
    pub per_version: &'static [&'static str],
}

/// プラグインの値を作らずに名前から引くのは，読むコマンドが設定ファイルを受け取らず，
/// Python や MSCCD の無いマシンでも記録を読めるようにするためである．
pub fn classifier_tables(name: &str) -> Result<Option<ClassifierTables>> {
    let tables = |sql, labels, per_version| {
        Some(ClassifierTables {
            sql,
            labels,
            per_version,
        })
    };
    match name {
        "claim" => Ok(tables(CLAIM_SQL, true, &["claim_context_issue"])),
        "fixture" => Ok(tables(FIXTURE_SQL, true, &["fixture_context_issue"])),
        "test_path" => Ok(tables(TEST_PATH_SQL, true, &[])),
        "generated" | "vendor" => Ok(None),
        // 記録を作ったツールより古いツールで読んだときに当たる．
        _ => bail!("no table definition is known for classifier `{name}`"),
    }
}

pub fn side_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(db.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// ロックを外す関数を持たないのは，捨てると OS が外すからである．
pub struct DbLock {
    _file: File,
}

pub fn lock(db: &Path) -> Result<DbLock> {
    let path = side_path(db, ".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(DbLock { _file: file }),
        Err(rustix::io::Errno::WOULDBLOCK) => {
            bail!("another cgweave record is using {}", db.display())
        }
        Err(err) => Err(err).with_context(|| format!("cannot lock {}", path.display())),
    }
}

/// create_schema のトランザクションの中で設定しないのは，ジャーナルの切り替えが
/// トランザクションの外でしかできないからである．
fn set_write_pragmas(conn: &Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        bail!("cannot switch the journal to WAL; SQLite kept {mode}");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

/// SQLite に作らせずに create_new で作るのは，あるかを見てから作るまでの間に誰かが同じ名前の
/// ファイルを作っていても，上書きせずに止めるためである．
pub fn create(db: &Path) -> Result<Connection> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(db)
        .with_context(|| format!("cannot create database {}", db.display()))?;
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("cannot open database {}", db.display()))?;
    set_write_pragmas(&conn).with_context(|| format!("cannot set up database {}", db.display()))?;
    Ok(conn)
}

pub fn create_schema(tx: &Transaction, classifiers: &[&str]) -> Result<()> {
    tx.execute_batch(EVENTS_SQL)
        .context("cannot create the event tables")?;
    for name in classifiers {
        if let Some(tables) = classifier_tables(name)? {
            tx.execute_batch(tables.sql)
                .with_context(|| format!("cannot create the tables of classifier `{name}`"))?;
        }
    }
    tx.pragma_update(None, "user_version", EVENTS_SCHEMA_VERSION)?;
    tx.pragma_update(None, "application_id", EVENTS_APPLICATION_ID)?;
    Ok(())
}

/// SQLite で開かずにファイルの先頭を読むのは，WAL のデータベースを読むだけの接続で開いても `-shm` と `-wal` ができ，
/// record がジャーナルを DELETE に戻すのを 1 回止めるからである．
///
/// 先頭の 19 と 20 バイト目は，SQLite のファイルの形の書く版と読む版で，WAL のときはどちらも 2 になる
pub fn is_wal(db: &Path) -> Result<bool> {
    use std::io::Read;
    let mut header = Vec::with_capacity(20);
    File::open(db)
        .and_then(|file| file.take(20).read_to_end(&mut header))
        .with_context(|| format!("cannot open database {}", db.display()))?;
    Ok(header.len() == 20 && header.starts_with(b"SQLite format 3\0") && header[18..20] == [2, 2])
}

/// WAL にしないのは，系譜のファイルを 1 回のトランザクションで書き，書き終えた後に 1 つのファイルで渡すためである
pub fn create_genealogy(path: &Path) -> Result<Connection> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("cannot open {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

pub fn create_genealogy_schema(tx: &Transaction) -> Result<()> {
    tx.execute_batch(GENEALOGY_SQL)
        .context("cannot create the genealogy tables")?;
    tx.pragma_update(None, "user_version", GENEALOGY_SCHEMA_VERSION)?;
    tx.pragma_update(None, "application_id", GENEALOGY_APPLICATION_ID)?;
    Ok(())
}

/// create と別にするのは，再開ではファイルを作らずに，すでにあるファイルを開くからである
pub fn open_write(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("cannot open database {}", db.display()))?;
    set_write_pragmas(&conn).with_context(|| format!("cannot set up database {}", db.display()))?;
    Ok(conn)
}

pub fn open_read(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open database {}", db.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// 1 つの読み取りのトランザクションで読み，接続を閉じてから返すのは，別の record が書き足しても
/// 同じ時点の中身を読み，読む側が止まっている間に record がジャーナルを DELETE に戻せるようにするためである．
pub fn read_snapshot<T>(db: &Path, read: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
    let mut conn = open_read(db)?;
    let result = {
        let tx = conn.transaction()?;
        check_format(&tx)?;
        let result = read(&tx)?;
        tx.finish()?;
        result
    };
    conn.close().map_err(|(_, err)| err)?;
    Ok(result)
}

pub fn check_format(conn: &Connection) -> Result<()> {
    let application_id: i32 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application_id == 0 && user_version == 0 {
        bail!(
            "the database has no tables; a record stopped before creating them. \
             Delete the file and run record again"
        );
    }
    if application_id == GENEALOGY_APPLICATION_ID {
        bail!("this is a genealogy file, not an event database");
    }
    if application_id != EVENTS_APPLICATION_ID {
        bail!("this is not a cgweave event database (application_id {application_id:#010x})");
    }
    if user_version != EVENTS_SCHEMA_VERSION {
        bail!(
            "the database has schema version {user_version}, but this cgweave reads version {EVENTS_SCHEMA_VERSION}"
        );
    }
    let versions: Vec<i64> = conn
        .prepare("SELECT schema_version FROM run")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match versions.as_slice() {
        [v] if *v == EVENTS_SCHEMA_VERSION => Ok(()),
        [v] => {
            bail!("the run row has schema version {v}, but the file says {EVENTS_SCHEMA_VERSION}")
        }
        rows => bail!("the database has {} run rows instead of 1", rows.len()),
    }
}

pub struct RecordedVersion {
    pub id: i64,
    pub commit: String,
    pub status: String,
}

/// Git でなく記録の版の中から探すのは，前の実行の後にコミットが増えて短いハッシュが曖昧になっても，
/// 記録の中で 1 つに決まれば受け付けるためである．
pub fn find_version(conn: &Connection, text: &str) -> Result<RecordedVersion> {
    let hex = text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex || !(4..=64).contains(&text.len()) {
        bail!("{text:?} is not a commit hash");
    }
    let found: Vec<RecordedVersion> = conn
        .prepare("SELECT id, commit_id, status FROM version WHERE substr(commit_id, 1, ?1) = ?2")?
        .query_map(rusqlite::params![text.len() as i64, text], |row| {
            Ok(RecordedVersion {
                id: row.get(0)?,
                commit: row.get(1)?,
                status: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    match found.len() {
        1 => Ok(found.into_iter().next().expect("one version")),
        0 => bail!("no recorded version starts with {text}"),
        n => bail!("{text} is ambiguous; {n} recorded versions start with it"),
    }
}

pub fn new_run_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

pub fn format_time(unix_secs: i64) -> Result<String> {
    let time = OffsetDateTime::from_unix_timestamp(unix_secs)
        .with_context(|| format!("time {unix_secs} is out of the supported range"))?;
    format_utc(time)
}

/// format_time の形だけを受け付けるのは，記録の時刻をどれもこの形で書くからである
pub fn parse_time(text: &str) -> Result<i64> {
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    let time = time::PrimitiveDateTime::parse(text, &format).with_context(|| {
        format!("the record has a time {text:?} that is not like 2026-01-02T03:04:05Z")
    })?;
    Ok(time.assume_utc().unix_timestamp())
}

pub fn now() -> String {
    format_utc(OffsetDateTime::now_utc()).expect("the current time is formattable")
}

fn format_utc(time: OffsetDateTime) -> Result<String> {
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    Ok(time.to_offset(time::UtcOffset::UTC).format(&format)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストの一時ディレクトリ．捨てると中身ごと消す．
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "cgweave-store-{name}-{}-{}",
                std::process::id(),
                OffsetDateTime::now_utc().unix_timestamp_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn created(dir: &TempDir, classifiers: &[&str]) -> (PathBuf, Connection) {
        let db = dir.0.join("e.db");
        let mut conn = create(&db).unwrap();
        let tx = conn.transaction().unwrap();
        create_schema(&tx, classifiers).unwrap();
        tx.commit().unwrap();
        (db, conn)
    }

    fn insert_run(conn: &Connection, schema_version: i64) {
        conn.execute(
            "INSERT INTO run (run_id, schema_version, tool_version, git_version, repository,
                              config, graph_sha256, started_at, status)
             VALUES (?1, ?2, 't', 'g', '/r', 'c', 'h', '2026-11-12T00:00:00Z', 'running')",
            rusqlite::params![format!("id-{schema_version}"), schema_version],
        )
        .unwrap();
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn create_makes_the_tables_of_the_used_classifiers_only() {
        let dir = TempDir::new("tables");
        let (_, conn) = created(&dir, &["claim", "generated", "vendor"]);
        let tables = table_names(&conn);
        assert!(tables.contains(&"pair_event".to_string()));
        assert!(tables.contains(&"claim_pair_event".to_string()));
        assert!(tables.contains(&"claim_context_issue".to_string()));
        assert!(
            !tables
                .iter()
                .any(|t| t.starts_with("fixture_") || t.starts_with("test_path_"))
        );
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn create_refuses_an_existing_file() {
        let dir = TempDir::new("exists");
        let db = dir.0.join("e.db");
        std::fs::write(&db, b"keep me").unwrap();
        assert!(create(&db).is_err());
        assert_eq!(std::fs::read(&db).unwrap(), b"keep me");
    }

    #[test]
    fn check_format_accepts_a_database_with_one_run() {
        let dir = TempDir::new("ok");
        let (db, conn) = created(&dir, &[]);
        insert_run(&conn, EVENTS_SCHEMA_VERSION);
        drop(conn);
        check_format(&open_read(&db).unwrap()).unwrap();
    }

    #[test]
    fn check_format_stops_on_an_empty_file() {
        let dir = TempDir::new("empty");
        let db = dir.0.join("e.db");
        drop(create(&db).unwrap());
        let err = check_format(&open_read(&db).unwrap()).unwrap_err();
        assert!(err.to_string().contains("no tables"), "{err}");
    }

    #[test]
    fn the_journal_is_read_from_the_header_without_opening_the_database() {
        let dir = TempDir::new("is-wal");
        let db = dir.0.join("e.db");
        let conn = create(&db).unwrap();
        conn.execute_batch("CREATE TABLE t (x)").unwrap();
        drop(conn);
        // 閉じた後は -wal も -shm も無いが，ジャーナルは WAL のままである
        let files = || std::fs::read_dir(&dir.0).unwrap().count();
        assert_eq!(files(), 1);
        assert!(is_wal(&db).unwrap());
        assert_eq!(files(), 1);
        let conn = Connection::open(&db).unwrap();
        conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .unwrap();
        drop(conn);
        assert!(!is_wal(&db).unwrap());
        std::fs::write(&db, b"").unwrap();
        assert!(!is_wal(&db).unwrap());
        assert!(is_wal(&dir.0.join("none.db")).is_err());
    }

    #[test]
    fn check_format_stops_on_a_genealogy_file() {
        let dir = TempDir::new("genealogy");
        let db = dir.0.join("g.db");
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "application_id", GENEALOGY_APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let err = check_format(&conn).unwrap_err();
        assert!(err.to_string().contains("genealogy file"), "{err}");
    }

    #[test]
    fn check_format_stops_on_another_schema_version() {
        let dir = TempDir::new("version");
        let (_, conn) = created(&dir, &[]);
        conn.pragma_update(None, "user_version", EVENTS_SCHEMA_VERSION + 1)
            .unwrap();
        let err = check_format(&conn).unwrap_err();
        assert!(err.to_string().contains("schema version"), "{err}");
    }

    #[test]
    fn check_format_stops_without_a_run_row() {
        let dir = TempDir::new("norun");
        let (_, conn) = created(&dir, &[]);
        let err = check_format(&conn).unwrap_err();
        assert!(err.to_string().contains("0 run rows"), "{err}");
    }

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_dropped() {
        let dir = TempDir::new("lock");
        let db = dir.0.join("e.db");
        let first = lock(&db).unwrap();
        let err = lock(&db).err().expect("the second lock must fail");
        assert!(err.to_string().contains("another cgweave record"), "{err}");
        drop(first);
        // 1 回で取り直せると決めないのは，並んで動くテストが fork した子が，exec するまで
        // ロックのファイルの記述子を写して持ち，その間はロックが外れないからである
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lock(&db).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the lock is not released"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn every_built_in_classifier_has_a_matching_table_definition() {
        for name in crate::version::CLASSIFIERS {
            let Some(tables) = classifier_tables(name).unwrap() else {
                continue;
            };
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(EVENTS_SQL).unwrap();
            conn.execute_batch(tables.sql).unwrap();
            let names = table_names(&conn);
            for suffix in ["pair_event", "root_pair"] {
                assert_eq!(
                    names.contains(&format!("{name}_{suffix}")),
                    tables.labels,
                    "{name}_{suffix}"
                );
            }
            for table in tables.per_version {
                let first: String = conn
                    .query_row(
                        &format!("SELECT name FROM pragma_table_info('{table}') WHERE cid = 0"),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(first, "version", "{table}");
            }
            let defined = names
                .iter()
                .filter(|n| n.starts_with(&format!("{name}_")))
                .count();
            assert_eq!(
                defined,
                tables.per_version.len() + if tables.labels { 2 } else { 0 },
                "{name}"
            );
        }
        assert!(classifier_tables("service").is_err());
    }

    #[test]
    fn a_snapshot_reads_one_point_in_time_and_closes_before_returning() {
        let dir = TempDir::new("snapshot");
        let (db, conn) = created(&dir, &[]);
        insert_run(&conn, EVENTS_SCHEMA_VERSION);
        drop(conn);
        let writer = Connection::open(&db).unwrap();
        let counts = read_snapshot(&db, |tx| {
            let before: i64 = tx.query_row("SELECT count(*) FROM run", [], |r| r.get(0))?;
            writer.execute("DELETE FROM run", []).unwrap();
            let after: i64 = tx.query_row("SELECT count(*) FROM run", [], |r| r.get(0))?;
            Ok((before, after))
        })
        .unwrap();
        assert_eq!(counts, (1, 1));
        let mode: String = writer
            .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn side_paths_append_the_suffix() {
        assert_eq!(
            side_path(Path::new("/d/x.db"), ".lock"),
            PathBuf::from("/d/x.db.lock")
        );
    }

    #[test]
    fn times_are_utc_seconds() {
        assert_eq!(format_time(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(format_time(1_793_266_680).unwrap(), "2026-10-29T09:38:00Z");
        assert!(format_time(253_402_300_800).is_err()); // 10000-01-01
    }

    #[test]
    fn run_ids_are_lowercase_version_4() {
        let id = new_run_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_eq!(id, id.to_lowercase());
    }
}
