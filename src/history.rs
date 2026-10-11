//! Bounded asynchronous history. Disk trouble must not block sensor/protection ticks.
use crate::model::{Reading, WARNING_AMPS};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_PENDING: usize = 2048;
const PRUNE_INTERVAL: Duration = Duration::from_secs(600);
const APPLICATION_ID: i64 = 0x41504d31;
pub const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_millis(200);
pub fn path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("AstralPowerMonitor")
        .join("readings.sqlite")
}
pub fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
#[derive(Clone)]
enum Record {
    Sample {
        time: i64,
        gpu: String,
        reading: Reading,
    },
    Event {
        time: i64,
        value: Value,
    },
}
impl Record {
    fn time(&self) -> i64 {
        match self {
            Self::Sample { time, .. } | Self::Event { time, .. } => *time,
        }
    }
}
#[derive(Default, Clone)]
pub struct Status {
    pub error: Option<String>,
    pub dropped: u64,
    pub pending: usize,
    pub committed: u64,
}
enum Command {
    Record(Record),
    Flush(mpsc::Sender<Result<(), String>>),
    Finish(mpsc::Sender<Result<(), String>>),
}
#[derive(Clone)]
pub struct History {
    sender: mpsc::SyncSender<Command>,
    pub status: Arc<Mutex<Status>>,
    closing: Arc<AtomicBool>,
}
impl History {
    pub fn start(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING);
        let history = Self {
            sender,
            status: Arc::new(Mutex::new(Status::default())),
            closing: Arc::new(AtomicBool::new(false)),
        };
        let status = history.status.clone();
        std::thread::spawn(move || writer(path, receiver, status));
        history
    }
    pub fn sample(&self, gpu: &str, reading: Reading) {
        self.record(Record::Sample {
            time: unix_ms(),
            gpu: gpu.into(),
            reading,
        });
    }
    pub fn event(&self, value: Value) {
        self.record(Record::Event {
            time: unix_ms(),
            value,
        });
    }
    fn record(&self, record: Record) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.sender.try_send(Command::Record(record)) {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            status.dropped += 1;
            status.error = Some(format!("History queue unavailable: {error}"));
        }
    }
    pub fn flush(&self, timeout: Duration) -> Result<(), String> {
        self.barrier(timeout, false)
    }
    pub fn finish(&self, timeout: Duration) -> Result<(), String> {
        self.closing.store(true, Ordering::SeqCst);
        self.barrier(timeout, true)
    }
    fn barrier(&self, timeout: Duration, finish: bool) -> Result<(), String> {
        let result = self.wait_for_barrier(timeout, finish);
        if let Err(error) = &result {
            self.status.lock().unwrap_or_else(|p| p.into_inner()).error = Some(error.clone());
        }
        result
    }
    fn wait_for_barrier(&self, timeout: Duration, finish: bool) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.sender
            .try_send(if finish {
                Command::Finish(tx)
            } else {
                Command::Flush(tx)
            })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(timeout)
            .map_err(|e| format!("History flush not completed within deadline: {e}"))?
    }
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[derive(Default)]
struct Policy {
    warning: bool,
    last_warning_flush: Option<Instant>,
}
impl Policy {
    fn urgent(&mut self, record: &Record, now: Instant) -> bool {
        match record {
            Record::Event { .. } => true,
            Record::Sample { reading, .. } => {
                let max = reading.max_amps();
                let urgent = max >= 13.0
                    || (max >= WARNING_AMPS
                        && (!self.warning
                            || self
                                .last_warning_flush
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))));
                self.warning = max >= WARNING_AMPS;
                if urgent {
                    self.last_warning_flush = Some(now);
                }
                urgent
            }
        }
    }
}
fn writer(path: PathBuf, receiver: mpsc::Receiver<Command>, status: Arc<Mutex<Status>>) {
    let mut store: Option<Store> = None;
    let mut pending = VecDeque::new();
    let mut policy = Policy::default();
    let mut deadline = Instant::now();
    let mut retry: Option<Instant> = None;
    loop {
        let mut ack = None;
        let mut finish = false;
        let mut urgent = false;
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(60));
        match receiver.recv_timeout(wait) {
            Ok(Command::Record(record)) => {
                urgent = policy.urgent(&record, Instant::now());
                if pending.len() >= MAX_PENDING {
                    // Drop the oldest ordinary sample first, retaining event context.
                    let index = pending
                        .iter()
                        .position(|r| matches!(r, Record::Sample { .. }))
                        .unwrap_or(0);
                    pending.remove(index);
                    status.lock().unwrap_or_else(|p| p.into_inner()).dropped += 1;
                }
                pending.push_back(record);
            }
            Ok(Command::Flush(sender)) => {
                ack = Some(sender);
                urgent = true;
            }
            Ok(Command::Finish(sender)) => {
                ack = Some(sender);
                urgent = true;
                finish = true;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                urgent = true;
                finish = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if ack.is_some()
            || finish
            || ((urgent || now >= deadline) && retry.is_none_or(|t| now >= t))
        {
            let result = (|| {
                if store.is_none() {
                    store = Some(Store::open(&path)?);
                }
                store.as_mut().unwrap().flush(&pending, unix_ms())
            })();
            let mut state = status.lock().unwrap_or_else(|p| p.into_inner());
            match &result {
                Ok(()) => {
                    state.committed += pending.len() as u64;
                    pending.clear();
                    state.error = None;
                    retry = None;
                    deadline = now + Duration::from_secs(60);
                }
                Err(error) => {
                    state.error = Some(error.clone());
                    retry = Some(now + Duration::from_secs(10));
                    deadline = retry.unwrap();
                }
            }
            state.pending = pending.len();
            if finish {
                drop(state);
                // A successful Finish acknowledges after the SQLite handles close.
                drop(store.take());
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                }
                return;
            }
            if let Some(ack) = ack {
                let _ = ack.send(result);
            }
        }
        status.lock().unwrap_or_else(|p| p.into_inner()).pending = pending.len();
        if finish {
            break;
        }
    }
}

struct Store {
    db: Connection,
    last_prune: Option<Instant>,
    folder: PathBuf,
}
impl Store {
    fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut db = Connection::open(path).map_err(|e| e.to_string())?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(|e| e.to_string())?;
        let app: i64 = db
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if (app != 0 && app != APPLICATION_ID)
            || ![0, 1].contains(&version)
            || (version == 1 && app != APPLICATION_ID)
        {
            return Err("History database identity/schema is unsupported".into());
        }
        if version == 0 {
            let tables:i64=db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if tables != 0 {
                return Err("Refusing to modify an unrecognized history database".into());
            }
            db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")
                .map_err(|e| e.to_string())?;
        }
        let mode: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if mode != "wal" {
            return Err("SQLite WAL mode unavailable".into());
        }
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=1000; PRAGMA journal_size_limit=4194304;").map_err(|e|e.to_string())?;
        if version == 0 {
            schema(&mut db)?;
        }
        let mut store = Self {
            db,
            last_prune: None,
            folder: path.parent().ok_or("Invalid history path")?.to_path_buf(),
        };
        store.import_legacy(path, unix_ms())?;
        Ok(store)
    }
    fn flush(&mut self, pending: &VecDeque<Record>, now_ms: i64) -> Result<(), String> {
        let prune = self
            .last_prune
            .is_none_or(|t| t.elapsed() >= PRUNE_INTERVAL);
        if pending.is_empty() && !prune {
            return Ok(());
        }
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        for record in pending {
            insert(&tx, record)?;
        }
        if prune {
            let cutoff = now_ms.saturating_sub(DAY_MS);
            tx.execute("DELETE FROM samples WHERE unix_time_ms < ?1", [cutoff])
                .map_err(|e| e.to_string())?;
            tx.execute("DELETE FROM events WHERE unix_time_ms < ?1", [cutoff])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        // Keep the independent reason file on this background writer. Neither a
        // SQLite stall nor a reason-file stall can block the shutdown caller.
        if let Some((time, reason)) = pending.iter().rev().find_map(|record| match record {
            Record::Event { time, value } if value["action"] == "shutdown_about_to_request" => {
                value["reason"].as_str().map(|reason| (*time, reason))
            }
            _ => None,
        }) {
            let _ = crate::actions::save_marker(
                &self.folder.join("last_shutdown.json"),
                &serde_json::json!({"schema_version":1,"unix_time_ms":time,"reason":reason}),
            );
        }
        if prune {
            self.last_prune = Some(Instant::now());
            // Free pages are reused. Incremental vacuum avoids a full rewrite.
            // Maintenance failure after COMMIT must not cause duplicate inserts.
            let _ = self
                .db
                .execute_batch("PRAGMA incremental_vacuum(256); PRAGMA wal_checkpoint(PASSIVE);");
        }
        Ok(())
    }
    fn import_legacy(&mut self, path: &Path, now_ms: i64) -> Result<(), String> {
        let done: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='jsonl_import_v1'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if done.is_some() {
            return Ok(());
        }
        let folder = path.parent().ok_or("Invalid history path")?;
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let (mut imported, mut skipped) = (0u64, 0u64);
        for name in [
            "readings.3.jsonl",
            "readings.2.jsonl",
            "readings.1.jsonl",
            "readings.jsonl",
        ] {
            let file = match std::fs::File::open(folder.join(name)) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.to_string()),
            };
            if file.metadata().map_err(|e| e.to_string())?.len() > 6 * 1024 * 1024 {
                return Err(format!(
                    "Legacy {name} exceeds import limit; original preserved"
                ));
            }
            let mut reader = BufReader::new(file.take(6 * 1024 * 1024));
            let mut line = Vec::new();
            loop {
                line.clear();
                if reader
                    .read_until(b'\n', &mut line)
                    .map_err(|e| e.to_string())?
                    == 0
                {
                    break;
                }
                if line.len() > 64 * 1024 {
                    skipped += 1;
                    continue;
                }
                let record = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(from_json);
                if let Some(record) = record
                    .filter(|r| r.time() >= now_ms.saturating_sub(DAY_MS) && r.time() <= now_ms)
                {
                    insert(&tx, &record)?;
                    imported += 1;
                } else {
                    skipped += 1;
                }
            }
        }
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES('jsonl_import_v1',?1)",
            [format!("imported={imported}; skipped={skipped}")],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
}
fn schema(db: &mut Connection) -> Result<(), String> {
    let tx = db.transaction().map_err(|e| e.to_string())?;
    tx.execute_batch("CREATE TABLE samples(id INTEGER PRIMARY KEY,unix_time_ms INTEGER NOT NULL,gpu TEXT NOT NULL,connector_watts REAL NOT NULL,max_pin_amps REAL NOT NULL,
        p1_volts REAL NOT NULL,p1_amps REAL NOT NULL,p2_volts REAL NOT NULL,p2_amps REAL NOT NULL,p3_volts REAL NOT NULL,p3_amps REAL NOT NULL,
        p4_volts REAL NOT NULL,p4_amps REAL NOT NULL,p5_volts REAL NOT NULL,p5_amps REAL NOT NULL,p6_volts REAL NOT NULL,p6_amps REAL NOT NULL);
        CREATE INDEX samples_time ON samples(unix_time_ms);
        CREATE TABLE events(id INTEGER PRIMARY KEY,unix_time_ms INTEGER NOT NULL,type TEXT NOT NULL,action TEXT,details_json TEXT NOT NULL CHECK(json_valid(details_json)));
        CREATE INDEX events_time ON events(unix_time_ms);
        CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);").map_err(|e|e.to_string())?;
    let parts:Vec<_>=(1..=6).map(|pin|format!("SELECT id AS sample_id,unix_time_ms,gpu,{pin} AS pin,p{pin}_volts AS volts,p{pin}_amps AS amps,p{pin}_volts*p{pin}_amps AS watts FROM samples")).collect();
    tx.execute_batch(&format!("CREATE VIEW pin_readings AS {}; PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version=1;",parts.join(" UNION ALL "))).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}
fn insert(db: &Connection, record: &Record) -> Result<(), String> {
    match record {
        Record::Sample { time, gpu, reading } => {
            let p = &reading.pins;
            db.prepare_cached("INSERT INTO samples(unix_time_ms,gpu,connector_watts,max_pin_amps,p1_volts,p1_amps,p2_volts,p2_amps,p3_volts,p3_amps,p4_volts,p4_amps,p5_volts,p5_amps,p6_volts,p6_amps) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)")
                .map_err(|e|e.to_string())?.execute(params![time,gpu,reading.watts(),reading.max_amps(),p[0].volts,p[0].amps,p[1].volts,p[1].amps,p[2].volts,p[2].amps,p[3].volts,p[3].amps,p[4].volts,p[4].amps,p[5].volts,p[5].amps]).map_err(|e|e.to_string())?;
        }
        Record::Event { time, value } => {
            let kind = value["type"].as_str().unwrap_or("event");
            let action = value["action"].as_str();
            db.prepare_cached(
                "INSERT INTO events(unix_time_ms,type,action,details_json) VALUES(?1,?2,?3,?4)",
            )
            .map_err(|e| e.to_string())?
            .execute(params![time, kind, action, value.to_string()])
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
fn from_json(value: Value) -> Option<Record> {
    if value
        .get("schema_version")
        .is_some_and(|version| version != 1)
    {
        return None;
    }
    let time = value["unix_time_ms"].as_i64()?;
    if value["type"] != "sample" {
        return Some(Record::Event { time, value });
    }
    let gpu = value["gpu"].as_str()?.to_string();
    let pins = value["pins"].as_array()?;
    if pins.len() != 6 {
        return None;
    }
    let mut reading = Reading {
        pins: [crate::model::Pin {
            volts: 0.0,
            amps: 0.0,
        }; 6],
    };
    let mut seen = [false; 6];
    for pin in pins {
        let index = usize::try_from(pin["pin"].as_u64()?).ok()?.checked_sub(1)?;
        if index >= 6 || seen[index] {
            return None;
        }
        seen[index] = true;
        let volts = pin["volts"].as_f64()? as f32;
        let amps = pin["amps"].as_f64()? as f32;
        if !(1.0..=16.0).contains(&volts) || !(0.0..=30.0).contains(&amps) {
            return None;
        }
        reading.pins[index] = crate::model::Pin { volts, amps };
    }
    Some(Record::Sample { time, gpu, reading })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pin;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        folder: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let folder = std::env::temp_dir().join(format!(
                "astral-sqlite-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&folder).unwrap();
            let path = folder.join("readings.sqlite");
            Self { folder, path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only files in this freshly created fixture; no recursive removal.
            for entry in std::fs::read_dir(&self.folder).unwrap() {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                std::fs::remove_file(entry.path()).unwrap();
            }
            std::fs::remove_dir(&self.folder).unwrap();
        }
    }
    fn reading(amps: f32) -> Reading {
        Reading {
            pins: [Pin { volts: 12.0, amps }; 6],
        }
    }
    fn sample(time: i64, amps: f32) -> Record {
        Record::Sample {
            time,
            gpu: "Test GPU".into(),
            reading: reading(amps),
        }
    }
    fn count(db: &Connection, table: &str) -> i64 {
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    #[test]
    fn batch_preserves_every_sample_and_exposes_queryable_pin_view() {
        let f = Fixture::new();
        let mut store = Store::open(&f.path).unwrap();
        let now = unix_ms();
        let batch: VecDeque<_> = (0..120)
            .map(|i| sample(now + i * 500, 1.0 + i as f32 / 100.0))
            .collect();
        assert_eq!(count(&store.db, "samples"), 0);
        store.flush(&batch, now).unwrap();
        assert_eq!(count(&store.db, "samples"), 120);
        assert_eq!(count(&store.db, "pin_readings"), 720);
        let(v,a,w):(f64,f64,f64)=store.db.query_row("SELECT volts,amps,watts FROM pin_readings WHERE pin=5 ORDER BY unix_time_ms LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((v, a, w), (12.0, 1.0, 12.0));
        assert_eq!(
            store
                .db
                .query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .db
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        drop(store);
        let db = Connection::open(&f.path).unwrap();
        assert_eq!(count(&db, "samples"), 120);
        assert_eq!(
            db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
    #[test]
    fn retention_removes_only_records_strictly_older_than_one_day() {
        let f = Fixture::new();
        let mut store = Store::open(&f.path).unwrap();
        let now = unix_ms();
        let cutoff = now - DAY_MS;
        let mut batch = VecDeque::new();
        for time in [cutoff - 1, cutoff, now] {
            batch.push_back(sample(time, 1.0));
            batch.push_back(Record::Event {
                time,
                value: serde_json::json!({"type":"test"}),
            });
        }
        store.flush(&batch, now).unwrap();
        assert_eq!(count(&store.db, "samples"), 2);
        assert_eq!(count(&store.db, "events"), 2);
        assert_eq!(
            store
                .db
                .query_row("SELECT min(unix_time_ms) FROM samples", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            cutoff
        );
    }
    #[test]
    fn failed_transaction_rolls_back_and_retry_does_not_duplicate_events() {
        let f = Fixture::new();
        let mut store = Store::open(&f.path).unwrap();
        let now = unix_ms();
        store.db.execute_batch("CREATE TRIGGER refuse_sample BEFORE INSERT ON samples BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        let batch = VecDeque::from([
            Record::Event {
                time: now,
                value: serde_json::json!({"type":"test"}),
            },
            sample(now, 1.0),
        ]);
        assert!(store.flush(&batch, now).is_err());
        assert_eq!(count(&store.db, "events"), 0);
        assert_eq!(count(&store.db, "samples"), 0);
        store
            .db
            .execute_batch("DROP TRIGGER refuse_sample;")
            .unwrap();
        store.flush(&batch, now).unwrap();
        assert_eq!(count(&store.db, "events"), 1);
        assert_eq!(count(&store.db, "samples"), 1);
    }
    #[test]
    fn locked_database_can_retry_after_lock_is_released() {
        let f = Fixture::new();
        let mut store = Store::open(&f.path).unwrap();
        let other = Connection::open(&f.path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let batch = VecDeque::from([sample(unix_ms(), 1.0)]);
        assert!(store.flush(&batch, unix_ms()).is_err());
        assert_eq!(count(&store.db, "samples"), 0);
        other.execute_batch("ROLLBACK;").unwrap();
        store.flush(&batch, unix_ms()).unwrap();
        assert_eq!(count(&store.db, "samples"), 1);
    }
    #[test]
    fn legacy_import_is_once_only_keeps_original_and_skips_broken_tail() {
        let f = Fixture::new();
        let now = unix_ms();
        let pins: Vec<_> = (1..=6)
            .map(|pin| serde_json::json!({"pin":pin,"volts":12.0,"amps":pin as f32}))
            .collect();
        let valid = serde_json::json!({"schema_version":1,"type":"sample","unix_time_ms":now,"gpu":"Legacy GPU","pins":pins});
        let text = format!(
            "{}\n{}\n{{\"partial\":",
            valid,
            serde_json::json!({"type":"startup","unix_time_ms":now-DAY_MS-1})
        );
        let legacy = f.folder.join("readings.jsonl");
        std::fs::write(&legacy, &text).unwrap();
        let store = Store::open(&f.path).unwrap();
        assert_eq!(count(&store.db, "samples"), 1);
        assert_eq!(count(&store.db, "events"), 0);
        drop(store);
        let store = Store::open(&f.path).unwrap();
        assert_eq!(count(&store.db, "samples"), 1);
        assert_eq!(std::fs::read_to_string(legacy).unwrap(), text);
        assert_eq!(
            store
                .db
                .query_row("SELECT p5_amps FROM samples", [], |r| r.get::<_, f64>(0))
                .unwrap(),
            5.0
        );
    }
    #[test]
    fn unrecognized_database_is_preserved() {
        let f = Fixture::new();
        let db = Connection::open(&f.path).unwrap();
        db.execute_batch(
            "CREATE TABLE important(value TEXT); INSERT INTO important VALUES('keep');",
        )
        .unwrap();
        drop(db);
        assert!(Store::open(&f.path).is_err());
        let db = Connection::open(&f.path).unwrap();
        assert_eq!(count(&db, "important"), 1);
    }
    #[test]
    fn warning_and_critical_force_early_flush_without_per_poll_warning_commits() {
        let (mut policy, t) = (Policy::default(), Instant::now());
        assert!(!policy.urgent(&sample(0, 9.19), t));
        assert!(policy.urgent(&sample(0, 9.2), t));
        assert!(!policy.urgent(&sample(0, 9.2), t + Duration::from_millis(500)));
        assert!(policy.urgent(&sample(0, 9.2), t + Duration::from_secs(1)));
        assert!(policy.urgent(&sample(0, 13.0), t + Duration::from_millis(1500)));
        assert!(!policy.urgent(&sample(0, 1.0), t + Duration::from_secs(2)));
        assert!(policy.urgent(&sample(0, 9.2), t + Duration::from_millis(2500)));
    }
    #[test]
    fn async_flush_barrier_and_finish_preserve_queued_records() {
        let f = Fixture::new();
        let history = History::start(f.path.clone());
        for _ in 0..120 {
            history.sample("Test GPU", reading(1.0));
        }
        history
            .event(serde_json::json!({"type":"protection","action":"shutdown_about_to_request","reason":"simulated request"}));
        history.flush(Duration::from_secs(5)).unwrap();
        let marker: Value =
            serde_json::from_slice(&std::fs::read(f.folder.join("last_shutdown.json")).unwrap())
                .unwrap();
        assert_eq!(marker["reason"], "simulated request");
        let db = Connection::open(&f.path).unwrap();
        assert_eq!(count(&db, "samples"), 120);
        history.sample("Test GPU", reading(2.0));
        history.finish(Duration::from_secs(5)).unwrap();
        assert_eq!(count(&db, "samples"), 121);
        history.sample("Ignored after closing", reading(3.0));
        assert_eq!(count(&db, "samples"), 121);
        drop(db);
        drop(history);
    }
    #[test]
    fn stalled_writer_has_bounded_queue_and_flush_deadline() {
        let (sender, _receiver) = mpsc::sync_channel(2);
        let history = History {
            sender,
            status: Arc::new(Mutex::new(Status::default())),
            closing: Arc::new(AtomicBool::new(false)),
        };
        let start = Instant::now();
        assert!(history.flush(Duration::from_millis(5)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        for _ in 0..4 {
            history.sample("Test GPU", reading(1.0));
        }
        assert_eq!(history.status().dropped, 3);
    }
    #[test]
    fn latest_sqlite_includes_wal_reset_fix() {
        assert!(rusqlite::version_number() >= 3051003);
    }
}
