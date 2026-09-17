//! Throughput of the browser's WebSockets, per target, socket and timeframe, kept in SQLite,
//! and the rate they move at right now.
//!
//! Only the hop between the browser and this gateway is measured: `/ws`, `/ws/audio`,
//! `/ws/camera` and `/ws/mic` each add the bytes of the data frames they write and read to
//! the [`Counter`] of the target the session has selected at that moment (see `crate::ws`
//! and [`crate::session::SessionManager::selected_target`]), or of no target while the
//! browser is on the picker. What an engine exchanges with its remote is a different link
//! and is not counted here.
//!
//! Once a second the counters are taken ([`ThroughputMeters::sample`]): what moved in that
//! second is the rate right now, which the browser reads through `GET /api/throughput/live`
//! ([`ThroughputMeters::live`]), and it is added to the open timeframe, which also keeps its
//! busiest second per direction, and the second itself where it moved anything. Every
//! [`TIMEFRAME`] the open timeframe is closed and each target's socket that moved data in
//! it gets one row, carrying its bytes, its peaks and its seconds; one that moved nothing
//! gets none, so idle hours cost no rows. The timeframe is only how often the seconds
//! reach the database — the meter's own resolution is one second and is not configurable. Each target's socket keeps its newest
//! `[meter].max_records` rows and the oldest go first. The browser reads them on demand
//! through `GET /api/throughput` ([`ThroughputStore::records`]), together with the open timeframe
//! as it stands and the closed ones not yet written ([`ThroughputMeters::snapshot`]). The
//! gateway stores bytes, peaks, times and the seconds that moved; the page divides for
//! averages where it is given no seconds.
//!
//! The sampler and the writer are separate tasks: a closed timeframe's rows wait in the
//! meters until the writer has committed them, so a slow or failing write never holds
//! up a sample, and a read meanwhile sees the rows from memory. Best effort, on purpose:
//! the timeframe still being counted when the process stops is lost, and a write that
//! fails is retried with the next timeframe's. What reaches the database is never torn —
//! a timeframe's rows and the trim after them are one transaction.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use log::warn;
use rusqlite::{Connection, TransactionBehavior, params};
use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::{MissedTickBehavior, interval_at};

/// The resolved `[meter]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterConfig {
    /// The SQLite database the records live in. Its directory is created when missing.
    pub database: PathBuf,
    /// Records kept per target and socket.
    pub max_records: usize,
}

/// One of the browser's WebSockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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

    /// Both counts so far, left counting.
    fn peek(&self) -> (u64, u64) {
        (self.sent.load(Ordering::Relaxed), self.received.load(Ordering::Relaxed))
    }
}

/// A sampled second that moved something, as the page reads it:
/// `[at, sentPerSec, receivedPerSec]`, `at` being seconds from its timeframe's start.
/// A second that moved nothing is not one of these: it is simply missing, and reads as
/// zero, which is what keeps a timeframe of a busy socket around three hundred bytes
/// and one of a quiet socket a handful.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Second(pub u32, pub u64, pub u64);

/// What one socket has moved for one target in the open timeframe, up to the last
/// sample, in that sample, and in each second of it that moved.
#[derive(Clone, Debug, Default)]
struct Tally {
    sent: u64,
    received: u64,
    /// The busiest second so far, in bytes per second.
    peak_sent: u64,
    peak_received: u64,
    /// The last sample, in bytes per second: the rate right now.
    now_sent: u64,
    now_received: u64,
    /// Every second of the timeframe that moved, oldest first.
    seconds: Vec<Second>,
}

impl Tally {
    /// Add a sample of `sent` and `received` bytes moved over `secs`, ending `at`
    /// seconds into the timeframe. A sample that spans more than a second is that
    /// second's; the ones it skipped moved nothing as far as anything here can tell.
    fn add(&mut self, sent: u64, received: u64, secs: u64, at: u32) {
        self.sent += sent;
        self.received += received;
        self.now_sent = per_second(sent, secs);
        self.now_received = per_second(received, secs);
        self.peak_sent = self.peak_sent.max(self.now_sent);
        self.peak_received = self.peak_received.max(self.now_received);
        // A trickle that rounds to nothing a second is no second of the graph's: it is
        // in the timeframe's bytes, and a zero here would only cost a row space.
        if self.now_sent != 0 || self.now_received != 0 {
            self.seconds.push(Second(at, self.now_sent, self.now_received));
        }
    }

    fn moved(&self) -> bool {
        self.sent != 0 || self.received != 0
    }
}

/// How many closed records wait for the writer at most, however many the database keeps.
/// While writes keep failing the oldest go first, as they would from the database; a week
/// of records with all their seconds is memory held against a backlog nobody reads.
const UNWRITTEN_MAX: usize = 4096;

/// `bytes` over `secs`, rounded; a sample never spans less than a second.
fn per_second(bytes: u64, secs: u64) -> u64 {
    let secs = secs.max(1);
    (bytes + secs / 2) / secs
}

/// The open timeframe: what the samples so far have added up to, and when; and the
/// closed timeframes' records the writer has not committed yet.
#[derive(Debug)]
struct Open {
    /// When the timeframe began, in Unix seconds: the last close, or the start.
    since: u64,
    /// When the counters were last sampled, in Unix seconds.
    sampled_at: u64,
    /// One set per entry of `ThroughputMeters::targets`, then the picker's.
    tallies: Vec<[Tally; 4]>,
    /// Closed, not yet written, oldest first, each under the number it was closed as.
    unwritten: VecDeque<(u64, Record)>,
    /// The number the next closed record takes.
    next_closed: u64,
}

/// Every target's [`Counter`] for every socket, and one more set for the picker, with
/// the open timeframe their samples add up to. One per gateway, shared by every
/// connection, so a reattach keeps counting into the same place.
///
/// The counters are atomics the sockets add to without a lock; only the sampler, once a
/// second, the writer, and a read of the open timeframe or the live rate take the lock
/// on `open`.
#[derive(Debug)]
pub struct ThroughputMeters {
    /// The `[[targets]]` names, in the order [`crate::session::SessionManager`] indexes.
    targets: Vec<String>,
    /// One set per entry of `targets`, then the picker's.
    counters: Vec<[Counter; 4]>,
    open: Mutex<Open>,
    /// How many closed records wait for the writer at most: while writes keep failing,
    /// the oldest go first, as they would from the database.
    unwritten_cap: usize,
}

impl Default for ThroughputMeters {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ThroughputMeters {
    pub fn new(targets: Vec<String>) -> Self {
        Self::open_at(targets, unix_now())
    }

    /// Meters whose first timeframe began at `start` (Unix seconds).
    pub(crate) fn open_at(targets: Vec<String>, start: u64) -> Self {
        let slots = targets.len() + 1;
        let counters = (0..slots).map(|_| Default::default()).collect();
        let open = Open {
            since: start,
            sampled_at: start,
            tallies: vec![std::array::from_fn(|_| Tally::default()); slots],
            unwritten: VecDeque::new(),
            next_closed: 0,
        };
        Self { targets, counters, open: Mutex::new(open), unwritten_cap: usize::MAX }
    }

    /// Meters recorded into a database keeping `max_records` per target and socket:
    /// what waits for the writer is capped at what the database would keep of it, and at
    /// [`UNWRITTEN_MAX`] whichever is smaller.
    fn recorded(targets: Vec<String>, max_records: usize) -> Self {
        let mut meters = Self::new(targets);
        let kept = max_records.saturating_mul(meters.counters.len() * Socket::ALL.len());
        meters.unwritten_cap = kept.min(UNWRITTEN_MAX);
        meters
    }

    /// The counter for `socket` under the target at `target` in the `[[targets]]` list, or
    /// under no target for `None` — and for an index the list does not have.
    pub fn counter(&self, target: Option<usize>, socket: Socket) -> &Counter {
        let slot = target.filter(|&index| index < self.targets.len()).unwrap_or(self.targets.len());
        &self.counters[slot][socket as usize]
    }

    fn lock_open(&self) -> std::sync::MutexGuard<'_, Open> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take every counter into the open timeframe as one sample ending at `now` (Unix
    /// seconds): what moved since the last sample, over the seconds since it, is the
    /// rate right now and a candidate for the timeframe's busiest second.
    pub(crate) fn sample(&self, now: u64) {
        let mut open = self.lock_open();
        Self::sample_into(&self.counters, &mut open, now);
    }

    fn sample_into(counters: &[[Counter; 4]], open: &mut Open, now: u64) {
        let secs = now.saturating_sub(open.sampled_at);
        let at = u32::try_from(now.saturating_sub(open.since)).unwrap_or(u32::MAX);
        for (slot, counters) in counters.iter().enumerate() {
            for socket in Socket::ALL {
                let (sent, received) = counters[socket as usize].take();
                open.tallies[slot][socket as usize].add(sent, received, secs, at);
            }
        }
        open.sampled_at = now.max(open.sampled_at);
    }

    /// End the open timeframe at `end` (Unix seconds), with a last sample, and begin the
    /// next one there: a record for each target's socket that moved data in it, queued
    /// for the writer and returned. The rate right now carries over; the sums and peaks
    /// start again.
    pub(crate) fn close_timeframe(&self, end: u64) -> Vec<Record> {
        let mut open = self.lock_open();
        Self::sample_into(&self.counters, &mut open, end);
        let records = self.records(&open, end);
        for tally in open.tallies.iter_mut().flatten() {
            *tally = Tally { now_sent: tally.now_sent, now_received: tally.now_received, ..Default::default() };
        }

        open.since = end;
        for record in &records {
            let number = open.next_closed;
            open.next_closed += 1;
            open.unwritten.push_back((number, record.clone()));
        }
        while open.unwritten.len() > self.unwritten_cap {
            open.unwritten.pop_front();
        }
        records
    }

    /// The closed records waiting for the writer, oldest first, and the number of the
    /// newest, to pass to [`Self::written`] once they are in the database.
    pub(crate) fn unwritten(&self) -> (u64, Vec<Record>) {
        let open = self.lock_open();
        let through = open.unwritten.back().map_or(0, |(number, _)| *number);
        (through, open.unwritten.iter().map(|(_, record)| record.clone()).collect())
    }

    /// The records closed as `through` and before are in the database.
    pub(crate) fn written(&self, through: u64) {
        let mut open = self.lock_open();
        while open.unwritten.front().is_some_and(|(number, _)| *number <= through) {
            open.unwritten.pop_front();
        }
    }

    /// The open timeframe as it stands at `now` (Unix seconds), left counting: a record
    /// for each target's socket that has moved data since the last close, including
    /// what has moved since the last sample.
    pub fn open_timeframe(&self, now: u64) -> Vec<Record> {
        self.snapshot(now).open
    }

    /// The open timeframe as it stands at `now` (see [`Self::open_timeframe`]) and the
    /// closed records not yet written, taken together under one lock: a close cannot
    /// fall between them.
    pub fn snapshot(&self, now: u64) -> Snapshot {
        let open = self.lock_open();
        let tallies = open
            .tallies
            .iter()
            .zip(&self.counters)
            .map(|(tallies, counters)| {
                std::array::from_fn(|socket| {
                    let (sent, received) = counters[socket].peek();
                    let tally = &tallies[socket];
                    Tally {
                        sent: tally.sent + sent,
                        received: tally.received + received,
                        ..tally.clone()
                    }
                })
            })
            .collect();
        let peeked = Open {
            since: open.since,
            sampled_at: open.sampled_at,
            tallies,
            unwritten: VecDeque::new(),
            next_closed: 0,
        };
        Snapshot {
            open: self.records(&peeked, now),
            unwritten: open.unwritten.iter().map(|(_, record)| record.clone()).collect(),
        }
    }

    /// A record per tally of `open` that moved, for the timeframe `open.since..end`. A
    /// clock set back cannot make a timeframe end before it began.
    fn records(&self, open: &Open, end: u64) -> Vec<Record> {
        let start = open.since;
        let end = end.max(start);
        let mut records = Vec::new();
        for (slot, tallies) in open.tallies.iter().enumerate() {
            for socket in Socket::ALL {
                let tally = &tallies[socket as usize];
                if !tally.moved() {
                    continue;
                }
                records.push(Record {
                    target: self.targets.get(slot).cloned(),
                    socket,
                    start,
                    end,
                    sent_bytes: tally.sent,
                    received_bytes: tally.received,
                    peak_sent_per_sec: tally.peak_sent,
                    peak_received_per_sec: tally.peak_received,
                    seconds: tally.seconds.clone(),
                });
            }
        }
        records
    }

    /// The rate right now: what the last sample found moving, per target and socket.
    pub fn live(&self) -> Live {
        let open = self.lock_open();
        let mut rates = Vec::new();
        for (slot, tallies) in open.tallies.iter().enumerate() {
            for socket in Socket::ALL {
                let tally = &tallies[socket as usize];
                if tally.now_sent == 0 && tally.now_received == 0 {
                    continue;
                }
                rates.push(LiveRate {
                    target: self.targets.get(slot).cloned(),
                    socket,
                    sent_per_sec: tally.now_sent,
                    received_per_sec: tally.now_received,
                });
            }
        }
        Live { at: open.sampled_at, rates }
    }
}

/// The meters at one moment: the open timeframe as it stands, and the closed records
/// the writer has not committed. Read before the database, so a timeframe closed
/// between the two is in the snapshot, and one written between the two is in both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub open: Vec<Record>,
    pub unwritten: Vec<Record>,
}

/// What a read of the throughput answers: the timeframes closed by then, oldest first, and
/// the one still being counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub records: Vec<Record>,
    pub open: Vec<Record>,
}

/// The seconds a read of the records covers: every timeframe that ended after `since`
/// and, when the read names an end, began before `until`. Unix seconds on the gateway's
/// own clock, which stamped the records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub since: u64,
    pub until: Option<u64>,
}

impl Window {
    /// Everything kept that ended after `since`, up to now.
    pub fn since(since: u64) -> Self {
        Self { since, until: None }
    }

    /// Whether a timeframe falls in this window: any part of it does.
    fn covers(&self, record: &Record) -> bool {
        record.end > self.since && self.until.is_none_or(|until| record.start < until)
    }
}

impl Snapshot {
    /// This snapshot beside `written`, the database's records read after it over the
    /// same `window`: an unwritten record the database has meanwhile is counted from the
    /// database, one it lacks is counted from here if the window covers it, and an open
    /// record the database has a row for — the timeframe closed and was written in
    /// between — yields to that row, as the whole of it.
    pub fn with_written(self, written: Vec<Record>, window: Window) -> Reading {
        let keys: HashSet<(Option<&str>, Socket, u64)> =
            written.iter().map(|record| (record.target.as_deref(), record.socket, record.start)).collect();
        let unwritten = |record: &Record| !keys.contains(&(record.target.as_deref(), record.socket, record.start));
        let kept = |record: &Record| window.covers(record) && unwritten(record);
        let mut records = written.clone();
        records.extend(self.unwritten.into_iter().filter(kept));
        let open = self.open.into_iter().filter(kept).collect();
        Reading { records, open }
    }
}

/// What one socket moved for one target in one timeframe, and its busiest second in
/// bytes per second. Times are Unix seconds; a `None` target is the picker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub target: Option<String>,
    pub socket: Socket,
    pub start: u64,
    pub end: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub peak_sent_per_sec: u64,
    pub peak_received_per_sec: u64,
    /// Every second of the timeframe that moved something, oldest first; empty in a read
    /// whose range is too long for them (see [`TRACE_SPAN_SECS`]), where a point on the
    /// graph is the timeframe's average instead.
    pub seconds: Vec<Second>,
}

/// The rate right now, as of the sample at `at` (Unix seconds): every target's socket
/// that moved in it, in bytes per second.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    pub at: u64,
    pub rates: Vec<LiveRate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRate {
    pub target: Option<String>,
    pub socket: Socket,
    pub sent_per_sec: u64,
    pub received_per_sec: u64,
}

/// A gateway's throughput: the counters the sockets add to, and the database they are
/// recorded in when `[meter]` is set.
#[derive(Clone, Debug, Default)]
pub struct Throughput {
    pub meters: Arc<ThroughputMeters>,
    pub store: Option<Arc<ThroughputStore>>,
}

/// Marks a database as this module's, in the header field SQLite keeps for it.
const APPLICATION_ID: i64 = 0x524d_5855; // "RMXU"
/// The one schema there is. A database written by any other is refused, not migrated.
const SCHEMA_VERSION: i64 = 5;
const SCHEMA: &str = "
    CREATE TABLE throughput (
        id INTEGER PRIMARY KEY,
        target TEXT,
        socket TEXT NOT NULL CHECK (socket IN ('session', 'audio', 'camera', 'mic')),
        started_at INTEGER NOT NULL,
        ended_at INTEGER NOT NULL,
        sent_bytes INTEGER NOT NULL CHECK (sent_bytes >= 0),
        received_bytes INTEGER NOT NULL CHECK (received_bytes >= 0),
        peak_sent_per_sec INTEGER NOT NULL CHECK (peak_sent_per_sec >= 0),
        peak_received_per_sec INTEGER NOT NULL CHECK (peak_received_per_sec >= 0),
        seconds BLOB NOT NULL
    ) STRICT;
    CREATE INDEX throughput_by_series ON throughput (target, socket, id);
    CREATE INDEX throughput_by_end ON throughput (ended_at);
";

/// How long one row's timeframe is: the seconds it groups, and so how often the seconds
/// reach the database. The meter itself is always one second — every second of a
/// timeframe that moved something is kept — so this is a flushing interval, not a
/// resolution, and it is not configurable.
pub const TIMEFRAME: Duration = Duration::from_secs(60);

/// The longest range a read is given the sampled seconds for. Past it a point on the
/// graph spans more than a timeframe anyway, and the seconds would be more bytes than
/// the page has pixels to draw them on.
pub const TRACE_SPAN_SECS: u64 = 3600;

/// The sampled seconds as one blob: for each, its offset into the timeframe, its sent
/// rate and its received rate, one unsigned LEB128 each, ascending by offset. A socket
/// busy through a minute takes some three hundred bytes; one that moved in three of its
/// seconds takes a dozen, and one that moved in none has no row at all.
fn encode_seconds(seconds: &[Second]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(seconds.len() * 4);
    for &Second(at, sent, received) in seconds {
        for number in [u64::from(at), sent, received] {
            let mut value = number;
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    blob.push(byte);
                    break;
                }
                blob.push(byte | 0x80);
            }
        }
    }
    blob
}

/// The seconds back out of a blob [`encode_seconds`] wrote. A blob that ends mid-number,
/// or holds one too large to be a rate, is a corrupt row, not a rate of zero.
fn decode_seconds(mut blob: &[u8]) -> anyhow::Result<Vec<Second>> {
    let number = |blob: &mut &[u8]| -> anyhow::Result<u64> {
        let mut value: u64 = 0;
        for (index, &byte) in blob.iter().enumerate() {
            let shift = 7 * index as u32;
            anyhow::ensure!(shift < 64, "a throughput record's seconds hold a number too large");
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                *blob = &blob[index + 1..];
                return Ok(value);
            }
        }
        anyhow::bail!("a throughput record's seconds end mid-number")
    };
    let mut seconds = Vec::new();
    while !blob.is_empty() {
        let at = number(&mut blob)?;
        let (sent, received) = (number(&mut blob)?, number(&mut blob)?);
        let at = u32::try_from(at).context("a throughput record's second is too far into it")?;
        seconds.push(Second(at, sent, received));
    }
    Ok(seconds)
}

/// The throughput database, open for the life of the gateway.
///
/// One connection behind a mutex, used from blocking tasks only: the recorder writes a
/// timeframe a minute and the browser reads on demand, so there is nothing to pool.
#[derive(Debug)]
pub struct ThroughputStore {
    connection: Mutex<Connection>,
    pub max_records: usize,
}

impl ThroughputStore {
    /// Open (or create) the database and trim it to `max_records`.
    ///
    /// A file that is not this module's database — not SQLite at all, another program's
    /// SQLite, or another schema version — is refused before anything is written to it,
    /// so a mistyped path never damages what is there.
    pub fn open(config: &MeterConfig) -> anyhow::Result<Self> {
        let path = &config.database;
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        // Only a file this call creates gets a schema. SQLite shows an existing empty file,
        // or another program's empty database, just like a new one, and neither is ours.
        // A new file is private to the gateway's user; SQLite gives the `-wal` and `-shm`
        // files it creates beside it the database's own permissions.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let created = match options.open(path) {
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

        let store = Self { connection: Mutex::new(connection), max_records: config.max_records };
        // A cap lowered between runs applies to what is already there.
        store.write(&[])?;
        Ok(store)
    }

    /// Check the database is this module's, creating the schema when `created` says the
    /// file is the one [`Self::open`] just made.
    fn adopt(connection: &mut Connection, path: &Path, created: bool) -> anyhow::Result<()> {
        let not_ours = || format!("{} is not a remotex throughput database", path.display());
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
                .with_context(|| format!("cannot create the throughput schema in {}", path.display()))?;
            return Ok(());
        }
        anyhow::ensure!(application_id == APPLICATION_ID, "{}", not_ours());
        let version: i64 =
            transaction.query_row("PRAGMA user_version", [], |row| row.get(0)).with_context(not_ours)?;
        anyhow::ensure!(
            version == SCHEMA_VERSION,
            "{} holds throughput schema {version}, and this gateway reads only {SCHEMA_VERSION} — \
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
        let transaction = connection.transaction().context("cannot begin a throughput write")?;
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO throughput (target, socket, started_at, ended_at, sent_bytes, received_bytes,
                                        peak_sent_per_sec, peak_received_per_sec, seconds)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .context("cannot prepare the throughput insert")?;
            for record in records {
                insert
                    .execute(params![
                        record.target,
                        record.socket.name(),
                        sql_int(record.start)?,
                        sql_int(record.end)?,
                        sql_int(record.sent_bytes)?,
                        sql_int(record.received_bytes)?,
                        sql_int(record.peak_sent_per_sec)?,
                        sql_int(record.peak_received_per_sec)?,
                        encode_seconds(&record.seconds),
                    ])
                    .context("cannot insert a throughput record")?;
            }

            let mut series: Vec<(Option<String>, String)> = if records.is_empty() {
                let mut select = transaction
                    .prepare_cached("SELECT DISTINCT target, socket FROM throughput")
                    .context("cannot prepare the throughput series query")?;
                select
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .and_then(Iterator::collect)
                    .context("cannot list the throughput series")?
            } else {
                records.iter().map(|record| (record.target.clone(), record.socket.name().to_owned())).collect()
            };
            series.sort();
            series.dedup();
            let mut trim = transaction
                .prepare_cached(
                    "DELETE FROM throughput WHERE target IS ?1 AND socket = ?2 AND id <= (
                         SELECT id FROM throughput WHERE target IS ?1 AND socket = ?2
                         ORDER BY id DESC LIMIT 1 OFFSET ?3
                     )",
                )
                .context("cannot prepare the throughput trim")?;
            let keep = i64::try_from(self.max_records).unwrap_or(i64::MAX);
            for (target, socket) in series {
                trim.execute(params![target, socket, keep]).context("cannot trim throughput records")?;
            }
        }
        transaction.commit().context("cannot commit a throughput write")
    }

    /// Every record the `window` covers, oldest first, with the seconds that moved in
    /// each when `seconds` asks for them — a range too long to draw them is read without
    /// so much as decoding them.
    pub fn records(&self, window: Window, seconds: bool) -> anyhow::Result<Vec<Record>> {
        let connection = self.lock();
        let mut select = connection
            .prepare_cached(
                "SELECT target, socket, started_at, ended_at, sent_bytes, received_bytes,
                        peak_sent_per_sec, peak_received_per_sec, seconds
                 FROM throughput WHERE ended_at > ?1 AND (?2 IS NULL OR started_at < ?2) ORDER BY id",
            )
            .context("cannot prepare the throughput query")?;
        let until = window.until.map(sql_int).transpose()?;
        let rows = select
            .query_map(params![sql_int(window.since)?, until], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    [
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ],
                    if seconds { row.get::<_, Vec<u8>>(8)? } else { Vec::new() },
                ))
            })
            .context("cannot query throughput records")?;
        rows.map(|row| {
            let (target, socket, numbers, blob) = row.context("cannot read a throughput record")?;
            let socket = Socket::from_name(&socket)
                .with_context(|| format!("a throughput record names no socket: {socket:?}"))?;
            let unsigned = |value: i64| u64::try_from(value).context("a throughput record is negative");
            let [start, end, sent, received, peak_sent, peak_received] = numbers;
            Ok(Record {
                target,
                socket,
                start: unsigned(start)?,
                end: unsigned(end)?,
                sent_bytes: unsigned(sent)?,
                received_bytes: unsigned(received)?,
                peak_sent_per_sec: unsigned(peak_sent)?,
                peak_received_per_sec: unsigned(peak_received)?,
                seconds: decode_seconds(&blob)?,
            })
        })
        .collect()
    }
}

/// SQLite integers are signed 64-bit; no byte count or timestamp here comes near the edge.
fn sql_int(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{value} does not fit an SQLite integer"))
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Meters for `targets` (the `[[targets]]` names, in order), and with `config` the
/// database they are recorded in; with no `[meter]`, counters nobody samples or records.
///
/// The database is opened and checked before this returns, so a path the gateway cannot
/// use fails the start instead of every write after it. After that nothing fails: a write
/// that does not succeed is logged and its records wait for the next attempt.
///
/// Two tasks: the sampler takes the counters every second and closes the timeframe
/// every [`TIMEFRAME`], never waiting on the database; the writer wakes at each close
/// and commits whatever is waiting, so a write that takes seconds, or SQLite's busy
/// wait, delays no sample and flattens no peak.
pub fn start(config: Option<&MeterConfig>, targets: Vec<String>) -> anyhow::Result<Throughput> {
    let Some(config) = config else {
        return Ok(Throughput { meters: Arc::new(ThroughputMeters::new(targets)), store: None });
    };
    let started = tokio::time::Instant::now();
    let store = Arc::new(ThroughputStore::open(config)?);
    let meters = Arc::new(ThroughputMeters::recorded(targets, store.max_records));
    let throughput = Throughput { meters: Arc::clone(&meters), store: Some(Arc::clone(&store)) };
    let closed = Arc::new(Notify::new());

    let sampler = Arc::clone(&meters);
    let wake = Arc::clone(&closed);
    tokio::spawn(async move {
        let mut next_close = started + TIMEFRAME;
        let mut ticks = interval_at(started + SAMPLE_PERIOD, SAMPLE_PERIOD);
        // A late tick is taken once, not bunched: the counters keep counting meanwhile,
        // and the sample after it is spread over the seconds it actually spans.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let tick = ticks.tick().await;
            if tick < next_close {
                sampler.sample(unix_now());
                continue;
            }
            sampler.close_timeframe(unix_now());
            while next_close <= tick {
                next_close += TIMEFRAME;
            }
            wake.notify_one();
        }
    });

    tokio::spawn(async move {
        loop {
            // A close during a write leaves a permit, so the next round begins at once.
            closed.notified().await;
            let (through, batch) = meters.unwritten();
            if batch.is_empty() {
                continue;
            }
            let writer = Arc::clone(&store);
            match tokio::task::spawn_blocking(move || writer.write(&batch)).await {
                Ok(Ok(())) => meters.written(through),
                Ok(Err(e)) => warn!("throughput: {e:#}"),
                Err(e) => warn!("throughput: the write task failed: {e}"),
            }
        }
    });
    Ok(throughput)
}

/// How often the counters are sampled: the second the rate right now is measured over.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;

    fn config(database: PathBuf, max_records: usize) -> MeterConfig {
        MeterConfig { database, max_records }
    }

    /// A one-minute record whose bytes all moved in the second after it began: its peaks
    /// are its bytes, and that one second is all it carries.
    fn record(target: Option<&str>, socket: Socket, start: u64, sent_bytes: u64, received_bytes: u64) -> Record {
        Record {
            target: target.map(str::to_owned),
            socket,
            start,
            end: start + 60,
            sent_bytes,
            received_bytes,
            peak_sent_per_sec: sent_bytes,
            peak_received_per_sec: received_bytes,
            seconds: vec![Second(1, sent_bytes, received_bytes)],
        }
    }

    fn live(target: Option<&str>, socket: Socket, sent_per_sec: u64, received_per_sec: u64) -> LiveRate {
        LiveRate { target: target.map(str::to_owned), socket, sent_per_sec, received_per_sec }
    }

    #[test]
    fn a_timeframe_records_each_target_and_socket_that_moved_data() {
        let meters = ThroughputMeters::open_at(vec!["mac".to_owned(), "win".to_owned()], 100);
        meters.counter(Some(1), Socket::Session).sent(1500);
        meters.counter(Some(1), Socket::Session).received(40);
        meters.counter(Some(0), Socket::Session).sent(3);
        meters.counter(None, Socket::Mic).received(900);
        meters.counter(Some(7), Socket::Audio).sent(5);
        // One second in: the first sample is what moved in it, and the rate right now.
        meters.sample(101);
        assert_eq!(
            meters.live(),
            Live {
                at: 101,
                rates: vec![
                    live(Some("mac"), Socket::Session, 3, 0),
                    live(Some("win"), Socket::Session, 1500, 40),
                    live(None, Socket::Audio, 5, 0),
                    live(None, Socket::Mic, 0, 900),
                ]
            },
            "an index past the target list counts as no target"
        );

        // A quieter second lowers the rate right now but not the timeframe's peak, and
        // what has moved since the last sample shows in the open timeframe.
        meters.counter(Some(1), Socket::Session).sent(600);
        meters.sample(102);
        assert_eq!(meters.live().rates, [live(Some("win"), Socket::Session, 600, 0)], "only what moved in that second");
        meters.counter(Some(1), Socket::Session).sent(7);
        let expected = [
            record(Some("mac"), Socket::Session, 100, 3, 0),
            Record {
                sent_bytes: 2107,
                peak_sent_per_sec: 1500,
                seconds: vec![Second(1, 1500, 40), Second(2, 600, 0)],
                ..record(Some("win"), Socket::Session, 100, 2107, 40)
            },
            record(None, Socket::Audio, 100, 5, 0),
            record(None, Socket::Mic, 100, 0, 900),
        ];
        assert_eq!(meters.open_timeframe(160), expected);
        // A close samples what is left over the seconds since the last sample: 7 bytes
        // over 58 seconds round to nothing, so the peak stands. The rate right now is the
        // last sample's, which found only those 7 bytes moving.
        assert_eq!(meters.close_timeframe(160), expected);
        assert_eq!(meters.live(), Live { at: 160, rates: vec![] });

        // The counters were taken, so an idle timeframe after it records nothing.
        assert_eq!(meters.open_timeframe(200), []);
        assert_eq!(meters.close_timeframe(220), []);

        // The next timeframe begins where the last one closed, and its peaks start over.
        meters.counter(Some(0), Socket::Audio).sent(8);
        assert_eq!(
            meters.open_timeframe(230),
            [Record {
                end: 230,
                peak_sent_per_sec: 0,
                // Nothing has been sampled since the close, so no second is named yet.
                seconds: vec![],
                ..record(Some("mac"), Socket::Audio, 220, 8, 0)
            }]
        );
        // A clock set back cannot end a timeframe before it began.
        assert_eq!(
            meters.open_timeframe(50),
            [Record {
                end: 220,
                peak_sent_per_sec: 0,
                seconds: vec![],
                ..record(Some("mac"), Socket::Audio, 220, 8, 0)
            }]
        );
    }

    #[test]
    fn closed_records_wait_for_the_writer_and_are_read_meanwhile() {
        let meters = ThroughputMeters::recorded(vec!["mac".to_owned()], 1);
        assert_eq!(meters.unwritten_cap, 8, "one record per target and socket, and the picker's");
        let start = meters.lock_open().since;
        meters.counter(Some(0), Socket::Session).sent(10);
        let first = meters.close_timeframe(start + 60);
        meters.counter(None, Socket::Audio).sent(20);
        let second = meters.close_timeframe(start + 120);
        let (through, waiting) = meters.unwritten();
        assert_eq!(waiting, [first.clone(), second.clone()].concat());
        assert_eq!(through, 1);

        // The read sees them with the open timeframe, and a database that has one of
        // them by then counts it once, from the database.
        meters.counter(Some(0), Socket::Mic).received(5);
        let snapshot = meters.snapshot(start + 130);
        assert_eq!(snapshot.unwritten, waiting);
        assert_eq!(snapshot.open.len(), 1);
        let open = snapshot.open.clone();
        let reading = snapshot.clone().with_written(first.clone(), Window::since(0));
        assert_eq!(reading, Reading { records: [first.clone(), second.clone()].concat(), open: open.clone() });
        let reading = snapshot.clone().with_written(vec![], Window::since(start + 60));
        assert_eq!(reading.records, second, "a record that ended by `since` is left out, as the database leaves it");
        // A window that ended before the read leaves out the timeframe still open, which
        // began after it, and keeps the closed ones that began inside it.
        let ended = Window { since: 0, until: Some(start + 120) };
        let reading = snapshot.clone().with_written(vec![], ended);
        assert_eq!(reading, Reading { records: [first.clone(), second.clone()].concat(), open: vec![] });
        // The open timeframe closed and was written between the snapshot and the
        // database read: the database's row is the whole of it, and the rows still
        // waiting are read from the snapshot after it.
        let written = Record { end: start + 180, received_bytes: 9, ..open[0].clone() };
        let reading = snapshot.with_written(vec![written.clone()], Window::since(0));
        assert_eq!(reading, Reading { records: [vec![written], first.clone(), second.clone()].concat(), open: vec![] });

        // Written through the first: the second still waits.
        meters.written(0);
        assert_eq!(meters.unwritten(), (1, second.clone()));
        meters.written(1);
        assert_eq!(meters.unwritten(), (0, vec![]), "nothing waits, so no number");

        // While writes fail, the oldest go first once the cap is reached.
        for timeframe in 0..10u64 {
            meters.counter(None, Socket::Session).sent(1);
            meters.close_timeframe(start + 200 + timeframe);
        }
        let (through, waiting) = meters.unwritten();
        assert_eq!(through, 12, "two records closed first: the microphone bytes left over, and the session");
        assert_eq!(waiting.iter().map(|record| record.end).collect::<Vec<_>>(), (start + 202..start + 210).collect::<Vec<_>>());
    }

    #[test]
    fn a_sample_spans_the_seconds_since_the_last_one() {
        let meters = ThroughputMeters::open_at(vec![], 100);
        meters.counter(None, Socket::Session).sent(1000);
        meters.sample(105);
        assert_eq!(meters.live(), Live { at: 105, rates: vec![live(None, Socket::Session, 200, 0)] });
        // Two samples in one second read as one second, never a division by nothing.
        meters.counter(None, Socket::Session).received(30);
        meters.sample(105);
        assert_eq!(meters.live().rates, [live(None, Socket::Session, 0, 30)]);
        assert_eq!(per_second(7, 58), 0);
        assert_eq!(per_second(29, 58), 1, "rounded, not truncated");
    }

    #[test]
    fn records_are_kept_across_a_reopen_and_read_from_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path().join("nested/meter.sqlite3"), 10);
        let first = record(Some("mac"), Socket::Session, 0, 2048, 12);
        let second = record(None, Socket::Audio, 60, 9000, 0);
        ThroughputStore::open(&config).unwrap().write(&[first.clone(), second.clone()]).unwrap();

        let store = ThroughputStore::open(&config).unwrap();
        assert_eq!(store.records(Window::since(0), true).unwrap(), [first.clone(), second.clone()]);
        assert_eq!(
            store.records(Window::since(60), true).unwrap(),
            std::slice::from_ref(&second),
            "a timeframe that ended by `since` is left out"
        );
        assert_eq!(
            store.records(Window::since(0), false).unwrap()[0].seconds,
            [],
            "a read of a range too long for them decodes none"
        );
        // A window of its own leaves out what began at its end as well.
        assert_eq!(store.records(Window { since: 0, until: Some(60) }, true).unwrap(), std::slice::from_ref(&first));
        assert_eq!(store.records(Window { since: 0, until: Some(61) }, true).unwrap(), [first, second]);
    }

    #[test]
    fn the_seconds_that_moved_are_a_blob_of_varints_and_the_quiet_ones_are_nothing() {
        assert_eq!(decode_seconds(&encode_seconds(&[])).unwrap(), []);
        let seconds = vec![Second(0, 1, 0), Second(7, 400_000, 127), Second(59, 0, u64::MAX)];
        assert_eq!(decode_seconds(&encode_seconds(&seconds)).unwrap(), seconds);
        assert_eq!(encode_seconds(&[Second(3, 5, 9)]).len(), 3, "small numbers take a byte each");
        let busy: Vec<Second> = (0..60).map(|at| Second(at, 400_000, 40_000)).collect();
        assert_eq!(encode_seconds(&busy).len(), 420, "a minute busy in every second is a few hundred bytes");
        assert_eq!(encode_seconds(&busy[..3]).len(), 21, "and a minute busy in three of them is nothing");
        for corrupt in [vec![0x80], vec![0x00, 0x00], vec![0xff; 11]] {
            assert!(decode_seconds(&corrupt).is_err(), "{corrupt:?}");
        }
    }

    #[test]
    fn each_target_and_socket_keeps_its_newest_records_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meter.sqlite3");
        let store = ThroughputStore::open(&config(path.clone(), 3)).unwrap();
        for timeframe in 0..5 {
            store.write(&[record(Some("mac"), Socket::Audio, timeframe, timeframe + 1, 0)]).unwrap();
        }
        store.write(&[record(Some("win"), Socket::Audio, 5, 70, 0)]).unwrap();
        store.write(&[record(None, Socket::Audio, 6, 80, 0)]).unwrap();
        store.write(&[record(Some("mac"), Socket::Camera, 7, 0, 7)]).unwrap();

        let sent = |store: &ThroughputStore, target: Option<&str>, socket| -> Vec<u64> {
            store
                .records(Window::since(0), true)
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
        let store = ThroughputStore::open(&config(path, 1)).unwrap();
        assert_eq!(sent(&store, Some("mac"), Socket::Audio), [5]);
        assert_eq!(store.records(Window::since(0), true).unwrap().len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_database_and_its_sidecars_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meter.sqlite3");
        let store = ThroughputStore::open(&config(path.clone(), 10)).unwrap();
        store.write(&[record(Some("mac"), Socket::Session, 0, 1, 1)]).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", file.display());
        }
    }

    #[test]
    fn a_file_that_is_not_a_throughput_database_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();

        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"not a database, but somebody's").unwrap();
        let error = ThroughputStore::open(&config(text.clone(), 1)).expect_err("not SQLite");
        assert!(format!("{error:#}").contains("is not a remotex throughput database"), "{error:#}");
        assert_eq!(std::fs::read(&text).unwrap(), b"not a database, but somebody's");

        let empty = dir.path().join("empty.sqlite3");
        std::fs::write(&empty, b"").unwrap();
        let error = ThroughputStore::open(&config(empty.clone(), 1)).expect_err("an existing empty file");
        assert!(format!("{error:#}").contains("is not a remotex throughput database"), "{error:#}");
        assert_eq!(std::fs::read(&empty).unwrap(), b"");

        let other = dir.path().join("other.sqlite3");
        Connection::open(&other).unwrap().execute_batch("CREATE TABLE throughput (x INTEGER)").unwrap();
        let before = std::fs::read(&other).unwrap();
        let error = ThroughputStore::open(&config(other.clone(), 1)).expect_err("another program's SQLite");
        assert!(format!("{error:#}").contains("is not a remotex throughput database"), "{error:#}");
        assert_eq!(std::fs::read(&other).unwrap(), before);

        let older = dir.path().join("older.sqlite3");
        let connection = Connection::open(&older).unwrap();
        connection.execute_batch("CREATE TABLE throughput (x INTEGER)").unwrap();
        connection.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        drop(connection);
        let error = ThroughputStore::open(&config(older, 1)).expect_err("another schema version");
        assert!(format!("{error:#}").contains("holds throughput schema 1"), "{error:#}");
    }
}
