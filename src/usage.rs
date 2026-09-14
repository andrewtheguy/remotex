//! Data usage of the browser's WebSockets, per target, socket and timeframe, kept in SQLite.
//!
//! Only the hop between the browser and this gateway is measured: `/ws`, `/ws/audio`,
//! `/ws/camera` and `/ws/mic` each add the bytes of the data frames they write and read to
//! the [`Counter`] of the target the session has selected at that moment (see `crate::ws`
//! and [`crate::session::SessionManager::selected_target`]), or of no target while the
//! browser is on the picker. What an engine exchanges with its remote is a different link
//! and is not counted here.
//!
//! Every `[usage].interval_secs` the counters are taken and each target's socket that
//! moved data in that timeframe gets one row; one that moved nothing gets none, so idle
//! hours cost no rows. Each target's socket keeps its newest `[usage].max_records` rows
//! and the oldest go first. The browser reads them on demand through `GET /api/usage`
//! ([`UsageStore::records`]).
//!
//! Best effort, on purpose: the timeframe still being counted when the process stops is
//! lost, and a write that fails is retried with the next timeframe's. What reaches the
//! database is never torn — a timeframe's rows and the trim after them are one transaction.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use log::warn;
use rusqlite::{Connection, TransactionBehavior, params};
use serde::Serialize;
use tokio::time::{MissedTickBehavior, interval_at};

/// The resolved `[usage]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageConfig {
    /// The SQLite database the records live in. Its directory is created when missing.
    pub database: PathBuf,
    /// The length of one timeframe, and how often it is written.
    pub interval: Duration,
    /// Records kept per target and socket.
    pub max_records: usize,
}

/// One of the browser's WebSockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Socket {
    Session,
    Audio,
    Camera,
    Mic,
}

impl Socket {
    const ALL: [Self; 4] = [Self::Session, Self::Audio, Self::Camera, Self::Mic];

    /// The name the database and the API spell it with.
    fn name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Audio => "audio",
            Self::Camera => "camera",
            Self::Mic => "mic",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|socket| socket.name() == name)
    }
}

/// Bytes one socket has moved for one target since its counters were last taken.
#[derive(Debug, Default)]
pub struct Counter {
    sent: AtomicU64,
    received: AtomicU64,
}

impl Counter {
    /// Bytes written to the browser.
    pub fn sent(&self, bytes: u64) {
        self.sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Bytes read from the browser.
    pub fn received(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Take both counts, leaving zero. A byte added between the two swaps lands in the
    /// next timeframe rather than nowhere.
    fn take(&self) -> (u64, u64) {
        (self.sent.swap(0, Ordering::Relaxed), self.received.swap(0, Ordering::Relaxed))
    }
}

/// Every target's [`Counter`] for every socket, and one more set for the picker. One per
/// gateway, shared by every connection, so a reattach keeps counting into the same place.
#[derive(Debug)]
pub struct UsageMeters {
    /// The `[[targets]]` names, in the order [`crate::session::SessionManager`] indexes.
    targets: Vec<String>,
    /// One set per entry of `targets`, then the picker's.
    counters: Vec<[Counter; 4]>,
}

impl Default for UsageMeters {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl UsageMeters {
    pub fn new(targets: Vec<String>) -> Self {
        let counters = (0..=targets.len()).map(|_| Default::default()).collect();
        Self { targets, counters }
    }

    /// The counter for `socket` under the target at `target` in the `[[targets]]` list, or
    /// under no target for `None` — and for an index the list does not have.
    pub fn counter(&self, target: Option<usize>, socket: Socket) -> &Counter {
        let slot = target.filter(|&index| index < self.targets.len()).unwrap_or(self.targets.len());
        &self.counters[slot][socket as usize]
    }

    /// End the timeframe `start..end`: take every counter and return a record for each
    /// target's socket that moved data.
    pub(crate) fn close_timeframe(&self, start: u64, end: u64) -> Vec<Record> {
        let mut records = Vec::new();
        for (slot, counters) in self.counters.iter().enumerate() {
            for socket in Socket::ALL {
                let (sent_bytes, received_bytes) = counters[socket as usize].take();
                if sent_bytes == 0 && received_bytes == 0 {
                    continue;
                }
                let target = self.targets.get(slot).cloned();
                records.push(Record { target, socket, start, end, sent_bytes, received_bytes });
            }
        }
        records
    }
}

/// What one socket moved for one target in one timeframe. Times are Unix seconds; a
/// `None` target is the picker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub target: Option<String>,
    pub socket: Socket,
    pub start: u64,
    pub end: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
}

/// A gateway's usage: the counters the sockets add to, and the database they are
/// recorded in when `[usage]` is set.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub meters: Arc<UsageMeters>,
    pub store: Option<Arc<UsageStore>>,
}

/// Marks a database as this module's, in the header field SQLite keeps for it.
const APPLICATION_ID: i64 = 0x524d_5855; // "RMXU"
/// The one schema there is. A database written by any other is refused, not migrated.
const SCHEMA_VERSION: i64 = 2;
const SCHEMA: &str = "
    CREATE TABLE usage (
        id INTEGER PRIMARY KEY,
        target TEXT,
        socket TEXT NOT NULL CHECK (socket IN ('session', 'audio', 'camera', 'mic')),
        started_at INTEGER NOT NULL,
        ended_at INTEGER NOT NULL,
        sent_bytes INTEGER NOT NULL CHECK (sent_bytes >= 0),
        received_bytes INTEGER NOT NULL CHECK (received_bytes >= 0)
    ) STRICT;
    CREATE INDEX usage_by_series ON usage (target, socket, id);
    CREATE INDEX usage_by_end ON usage (ended_at);
";

/// The usage database, open for the life of the gateway.
///
/// One connection behind a mutex, used from blocking tasks only: the recorder writes a
/// timeframe a minute and the browser reads on demand, so there is nothing to pool.
#[derive(Debug)]
pub struct UsageStore {
    connection: Mutex<Connection>,
    pub interval: Duration,
    pub max_records: usize,
}

impl UsageStore {
    /// Open (or create) the database and trim it to `max_records`.
    ///
    /// A file that is not this module's database — not SQLite at all, another program's
    /// SQLite, or another schema version — is refused before anything is written to it,
    /// so a mistyped path never damages what is there.
    pub fn open(config: &UsageConfig) -> anyhow::Result<Self> {
        let path = &config.database;
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        // Only a file this call creates gets a schema. SQLite shows an existing empty file,
        // or another program's empty database, just like a new one, and neither is ours.
        let created = match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
        };
        let adopted = Connection::open(path)
            .with_context(|| format!("cannot open {}", path.display()))
            .and_then(|mut connection| {
                connection.busy_timeout(Duration::from_secs(5)).context("cannot set the busy timeout")?;
                Self::adopt(&mut connection, path, created)?;
                Ok(connection)
            });
        let connection = match adopted {
            Ok(connection) => connection,
            Err(e) => {
                // A file left empty would be refused by every later start.
                if created {
                    let _ = std::fs::remove_file(path);
                }
                return Err(e);
            }
        };
        // Only once the file is known to be ours: switching the journal writes to it.
        let journal: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .with_context(|| format!("cannot set the journal mode of {}", path.display()))?;
        anyhow::ensure!(journal.eq_ignore_ascii_case("wal"), "{} refused WAL ({journal})", path.display());
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .context("cannot set synchronous = NORMAL")?;

        let store = Self {
            connection: Mutex::new(connection),
            interval: config.interval,
            max_records: config.max_records,
        };
        // A cap lowered between runs applies to what is already there.
        store.write(&[])?;
        Ok(store)
    }

    /// Check the database is this module's, creating the schema when `created` says the
    /// file is the one [`Self::open`] just made.
    fn adopt(connection: &mut Connection, path: &Path, created: bool) -> anyhow::Result<()> {
        let not_ours = || format!("{} is not a remotex usage database", path.display());
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(not_ours)?;
        let objects: i64 = transaction
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
            .with_context(not_ours)?;
        let application_id: i64 =
            transaction.query_row("PRAGMA application_id", [], |row| row.get(0)).with_context(not_ours)?;
        if created && objects == 0 && application_id == 0 {
            transaction
                .execute_batch(SCHEMA)
                .and_then(|()| transaction.pragma_update(None, "application_id", APPLICATION_ID))
                .and_then(|()| transaction.pragma_update(None, "user_version", SCHEMA_VERSION))
                .and_then(|()| transaction.commit())
                .with_context(|| format!("cannot create the usage schema in {}", path.display()))?;
            return Ok(());
        }
        anyhow::ensure!(application_id == APPLICATION_ID, "{}", not_ours());
        let version: i64 =
            transaction.query_row("PRAGMA user_version", [], |row| row.get(0)).with_context(not_ours)?;
        anyhow::ensure!(
            version == SCHEMA_VERSION,
            "{} holds usage schema {version}, and this gateway reads only {SCHEMA_VERSION} — \
             move the file away to start a new one",
            path.display()
        );
        let check: String = transaction
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .with_context(|| format!("cannot check {}", path.display()))?;
        anyhow::ensure!(check == "ok", "{} failed its integrity check: {check}", path.display());
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic inside a transaction rolled it back as it unwound; the connection is fine.
        self.connection.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add `records` and trim what they were added to back to `max_records`, all or
    /// nothing. No records trims every target's every socket.
    pub(crate) fn write(&self, records: &[Record]) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction().context("cannot begin a usage write")?;
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO usage (target, socket, started_at, ended_at, sent_bytes, received_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .context("cannot prepare the usage insert")?;
            for record in records {
                insert
                    .execute(params![
                        record.target,
                        record.socket.name(),
                        sql_int(record.start)?,
                        sql_int(record.end)?,
                        sql_int(record.sent_bytes)?,
                        sql_int(record.received_bytes)?,
                    ])
                    .context("cannot insert a usage record")?;
            }

            let mut series: Vec<(Option<String>, String)> = if records.is_empty() {
                let mut select = transaction
                    .prepare_cached("SELECT DISTINCT target, socket FROM usage")
                    .context("cannot prepare the usage series query")?;
                select
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .and_then(Iterator::collect)
                    .context("cannot list the usage series")?
            } else {
                records.iter().map(|record| (record.target.clone(), record.socket.name().to_owned())).collect()
            };
            series.sort();
            series.dedup();
            let mut trim = transaction
                .prepare_cached(
                    "DELETE FROM usage WHERE target IS ?1 AND socket = ?2 AND id <= (
                         SELECT id FROM usage WHERE target IS ?1 AND socket = ?2
                         ORDER BY id DESC LIMIT 1 OFFSET ?3
                     )",
                )
                .context("cannot prepare the usage trim")?;
            let keep = i64::try_from(self.max_records).unwrap_or(i64::MAX);
            for (target, socket) in series {
                trim.execute(params![target, socket, keep]).context("cannot trim usage records")?;
            }
        }
        transaction.commit().context("cannot commit a usage write")
    }

    /// Every record whose timeframe ended after `since` (Unix seconds), oldest first.
    pub fn records(&self, since: u64) -> anyhow::Result<Vec<Record>> {
        let connection = self.lock();
        let mut select = connection
            .prepare_cached(
                "SELECT target, socket, started_at, ended_at, sent_bytes, received_bytes
                 FROM usage WHERE ended_at > ?1 ORDER BY id",
            )
            .context("cannot prepare the usage query")?;
        let rows = select
            .query_map([sql_int(since)?], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .context("cannot query usage records")?;
        rows.map(|row| {
            let (target, socket, start, end, sent, received) = row.context("cannot read a usage record")?;
            let socket = Socket::from_name(&socket)
                .with_context(|| format!("a usage record names no socket: {socket:?}"))?;
            let unsigned = |value: i64| u64::try_from(value).context("a usage record is negative");
            Ok(Record {
                target,
                socket,
                start: unsigned(start)?,
                end: unsigned(end)?,
                sent_bytes: unsigned(sent)?,
                received_bytes: unsigned(received)?,
            })
        })
        .collect()
    }
}

/// SQLite integers are signed 64-bit; no byte count or timestamp here comes near the edge.
fn sql_int(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{value} does not fit an SQLite integer"))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Meters for `targets` (the `[[targets]]` names, in order), and with `config` the
/// database they are recorded in; with no `[usage]`, counters nobody records.
///
/// The database is opened and checked before this returns, so a path the gateway cannot
/// use fails the start instead of every write after it. After that nothing fails: a write
/// that does not succeed is logged and its records ride along with the next timeframe's.
pub fn start(config: Option<&UsageConfig>, targets: Vec<String>) -> anyhow::Result<Usage> {
    let meters = Arc::new(UsageMeters::new(targets));
    let Some(config) = config else {
        return Ok(Usage { meters, store: None });
    };
    // Before the database is touched. The config check refuses such an interval too.
    let first_tick = tokio::time::Instant::now()
        .checked_add(config.interval)
        .with_context(|| format!("[usage].interval_secs {} is too long to schedule", config.interval.as_secs()))?;
    let store = Arc::new(UsageStore::open(config)?);
    let usage = Usage { meters: Arc::clone(&meters), store: Some(Arc::clone(&store)) };
    // Kept while writes fail, up to what the database would keep of them anyway.
    let pending_cap = store.max_records.saturating_mul(meters.counters.len() * Socket::ALL.len());

    tokio::spawn(async move {
        let mut pending: Vec<Record> = Vec::new();
        let mut start = unix_now();
        let mut ticks = interval_at(first_tick, store.interval);
        // A slow write delays the next timeframe instead of bunching several up: the
        // counters keep counting meanwhile, so nothing is lost, only longer.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let end = unix_now();
            pending.extend(meters.close_timeframe(start, end));
            start = end;
            if pending.is_empty() {
                continue;
            }
            let excess = pending.len().saturating_sub(pending_cap);
            pending.drain(..excess);

            let batch = std::mem::take(&mut pending);
            let writer = Arc::clone(&store);
            match tokio::task::spawn_blocking(move || writer.write(&batch).map_err(|e| (e, batch))).await {
                Ok(Ok(())) => {}
                Ok(Err((e, batch))) => {
                    warn!("usage: {e:#}");
                    pending = batch;
                }
                Err(e) => warn!("usage: the write task failed: {e}"),
            }
        }
    });
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(database: PathBuf, max_records: usize) -> UsageConfig {
        UsageConfig { database, interval: Duration::from_secs(60), max_records }
    }

    fn record(target: Option<&str>, socket: Socket, start: u64, sent_bytes: u64, received_bytes: u64) -> Record {
        Record { target: target.map(str::to_owned), socket, start, end: start + 60, sent_bytes, received_bytes }
    }

    #[test]
    fn a_timeframe_records_each_target_and_socket_that_moved_data() {
        let meters = UsageMeters::new(vec!["mac".to_owned(), "win".to_owned()]);
        meters.counter(Some(1), Socket::Session).sent(1500);
        meters.counter(Some(1), Socket::Session).received(40);
        meters.counter(Some(0), Socket::Session).sent(3);
        meters.counter(None, Socket::Mic).received(900);
        meters.counter(Some(7), Socket::Audio).sent(5);

        assert_eq!(
            meters.close_timeframe(100, 160),
            [
                record(Some("mac"), Socket::Session, 100, 3, 0),
                record(Some("win"), Socket::Session, 100, 1500, 40),
                record(None, Socket::Audio, 100, 5, 0),
                record(None, Socket::Mic, 100, 0, 900),
            ],
            "an index past the target list counts as no target"
        );
        // The counters were taken, so an idle timeframe after it records nothing.
        assert_eq!(meters.close_timeframe(160, 220), []);
    }

    #[test]
    fn records_are_kept_across_a_reopen_and_read_from_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().join("nested/usage.sqlite3"), 10);
        let first = record(Some("mac"), Socket::Session, 0, 2048, 12);
        let second = record(None, Socket::Audio, 60, 9000, 0);
        UsageStore::open(&config).unwrap().write(&[first.clone(), second.clone()]).unwrap();

        let store = UsageStore::open(&config).unwrap();
        assert_eq!(store.records(0).unwrap(), [first, second.clone()]);
        assert_eq!(store.records(60).unwrap(), [second], "a timeframe that ended by `since` is left out");
    }

    #[test]
    fn each_target_and_socket_keeps_its_newest_records_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let store = UsageStore::open(&config(path.clone(), 3)).unwrap();
        for timeframe in 0..5 {
            store.write(&[record(Some("mac"), Socket::Audio, timeframe, timeframe + 1, 0)]).unwrap();
        }
        store.write(&[record(Some("win"), Socket::Audio, 5, 70, 0)]).unwrap();
        store.write(&[record(None, Socket::Audio, 6, 80, 0)]).unwrap();
        store.write(&[record(Some("mac"), Socket::Camera, 7, 0, 7)]).unwrap();

        let sent = |store: &UsageStore, target: Option<&str>, socket| -> Vec<u64> {
            store
                .records(0)
                .unwrap()
                .iter()
                .filter(|r| r.target.as_deref() == target && r.socket == socket)
                .map(|r| r.sent_bytes)
                .collect()
        };
        assert_eq!(sent(&store, Some("mac"), Socket::Audio), [3, 4, 5], "the oldest go first");
        assert_eq!(sent(&store, Some("win"), Socket::Audio), [70], "one target's cap is not another's");
        assert_eq!(sent(&store, None, Socket::Audio), [80], "nor the picker's");
        assert_eq!(sent(&store, Some("mac"), Socket::Camera).len(), 1, "nor another socket's");
        drop(store);

        // A cap lowered between runs applies to what the database already holds.
        let store = UsageStore::open(&config(path, 1)).unwrap();
        assert_eq!(sent(&store, Some("mac"), Socket::Audio), [5]);
        assert_eq!(store.records(0).unwrap().len(), 4);
    }

    #[test]
    fn a_file_that_is_not_a_usage_database_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();

        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"not a database, but somebody's").unwrap();
        let error = UsageStore::open(&config(text.clone(), 1)).expect_err("not SQLite");
        assert!(format!("{error:#}").contains("is not a remotex usage database"), "{error:#}");
        assert_eq!(std::fs::read(&text).unwrap(), b"not a database, but somebody's");

        let empty = dir.path().join("empty.sqlite3");
        std::fs::write(&empty, b"").unwrap();
        let error = UsageStore::open(&config(empty.clone(), 1)).expect_err("an existing empty file");
        assert!(format!("{error:#}").contains("is not a remotex usage database"), "{error:#}");
        assert_eq!(std::fs::read(&empty).unwrap(), b"");

        let other = dir.path().join("other.sqlite3");
        Connection::open(&other).unwrap().execute_batch("CREATE TABLE usage (x INTEGER)").unwrap();
        let before = std::fs::read(&other).unwrap();
        let error = UsageStore::open(&config(other.clone(), 1)).expect_err("another program's SQLite");
        assert!(format!("{error:#}").contains("is not a remotex usage database"), "{error:#}");
        assert_eq!(std::fs::read(&other).unwrap(), before);

        let older = dir.path().join("older.sqlite3");
        let connection = Connection::open(&older).unwrap();
        connection.execute_batch("CREATE TABLE usage (x INTEGER)").unwrap();
        connection.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        drop(connection);
        let error = UsageStore::open(&config(older, 1)).expect_err("another schema version");
        assert!(format!("{error:#}").contains("holds usage schema 1"), "{error:#}");
    }
}
