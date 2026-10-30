use super::StatsManager;
use anyhow::{bail, Result};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

const SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const ARCHIVES: usize = 3;
pub(super) enum Command {
    Wake,
    Clear(mpsc::SyncSender<std::result::Result<(), String>>),
    Shutdown(mpsc::SyncSender<std::result::Result<(), String>>),
    Stop,
}
fn directory(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join("telemetry")
}
fn active(db: &Path) -> PathBuf {
    directory(db).join("dictation-events.jsonl")
}
fn files(db: &Path) -> Result<Vec<PathBuf>> {
    let dir = directory(db);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "dictation-events.jsonl"
            || (name.starts_with("dictation-events-") && name.ends_with(".jsonl"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}
fn recover(db: &Path) -> Result<HashSet<String>> {
    fs::create_dir_all(directory(db))?;
    let mut ids = HashSet::new();
    for path in files(db)? {
        let is_active = path == active(db);
        let mut reader = BufReader::new(File::open(&path)?);
        let mut offset = 0;
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                if is_active {
                    let file = OpenOptions::new().write(true).open(&path)?;
                    file.set_len(offset)?;
                    file.sync_all()?;
                    break;
                }
                bail!("unterminated_archive");
            }
            let value: serde_json::Value = serde_json::from_slice(&line)?;
            let id = value
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("trace_run_id_missing"))?;
            ids.insert(id.to_owned());
            offset += count as u64;
        }
        File::open(path)?.sync_all()?;
    }
    Ok(ids)
}
fn finish_clear(db: &Path) -> Result<()> {
    for path in files(db)? {
        fs::remove_file(path)?;
    }
    if directory(db).exists() {
        sync_directory(&directory(db))?;
    }
    StatsManager::connect_path(db)?
        .execute("UPDATE telemetry_state SET clear_pending=0 WHERE id=1", [])?;
    Ok(())
}
fn clear(db: &Path, epoch: &AtomicU64) -> Result<()> {
    let mut conn = StatsManager::connect_path(db)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch("UPDATE telemetry_state SET generation=generation+1,clear_pending=1 WHERE id=1;DELETE FROM dictation_traces;DELETE FROM transcription_runs;")?;
    let generation = tx.query_row(
        "SELECT generation FROM telemetry_state WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    tx.commit()?;
    epoch.store(generation, Ordering::Release);
    finish_clear(db)?;
    Ok(())
}
fn export_pending(db: &Path, recovered: &mut bool) -> Result<()> {
    export_pending_limit(db, recovered, SEGMENT_BYTES)
}
fn export_pending_limit(db: &Path, recovered: &mut bool, segment_bytes: u64) -> Result<()> {
    let conn = StatsManager::connect_path(db)?;
    if conn.query_row(
        "SELECT clear_pending FROM telemetry_state WHERE id=1",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        finish_clear(db)?;
    }
    if !*recovered {
        let ids = recover(db)?;
        for id in ids {
            conn.execute(
                "UPDATE dictation_traces SET exported=1,payload=NULL WHERE run_id=?1",
                [id],
            )?;
        }
        *recovered = true;
    }
    loop {
        let mut stmt = conn.prepare(
            "SELECT run_id,payload FROM dictation_traces WHERE exported=0 ORDER BY rowid LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        let Some(row) = rows.next()? else {
            break;
        };
        let id: String = row.get(0)?;
        let payload: String = row.get(1)?;
        drop(rows);
        drop(stmt);
        let path = active(db);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        if file.metadata()?.len() > 0
            && file.metadata()?.len() + payload.len() as u64 + 1 > segment_bytes
        {
            file.sync_all()?;
            drop(file);
            let sequence:u64=conn.query_row("UPDATE telemetry_state SET next_segment=next_segment+1 WHERE id=1 RETURNING next_segment-1",[],|r|r.get(0))?;
            let segment = directory(db).join(format!("dictation-events-{sequence:020}.jsonl"));
            fs::rename(&path, segment)?;
            sync_directory(&directory(db))?;
            let archives = files(db)?
                .into_iter()
                .filter(|p| p != &path)
                .collect::<Vec<_>>();
            let excess = archives.len().saturating_sub(ARCHIVES);
            for old in archives.into_iter().take(excess) {
                fs::remove_file(old)?;
            }
            sync_directory(&directory(db))?;
            file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&path)?;
        }
        let before = file.metadata()?.len();
        if let Err(error) = (|| -> std::io::Result<()> {
            file.write_all(payload.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_all()
        })() {
            let _ = file.set_len(before);
            let _ = file.sync_all();
            return Err(error.into());
        }
        sync_directory(&directory(db))?;
        conn.execute(
            "UPDATE dictation_traces SET exported=1,payload=NULL WHERE run_id=?1",
            [id],
        )?;
    }
    Ok(())
}
pub(super) fn run(
    db: PathBuf,
    rx: mpsc::Receiver<Command>,
    error: Arc<Mutex<Option<String>>>,
    generation: Arc<AtomicU64>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut recovered = false;
    loop {
        match export_pending(&db, &mut recovered) {
            Ok(()) => {
                *error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                recovered = false;
                *error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("trace_export_failed: {e}"));
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
        match rx.recv_timeout(backoff) {
            Ok(Command::Clear(reply)) => {
                let result = clear(&db, &generation).map_err(|e| e.to_string());
                if let Err(e) = &result {
                    *error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("trace_clear_failed: {e}"));
                }
                let _ = reply.send(result);
            }
            Ok(Command::Shutdown(reply)) => {
                let result = export_pending(&db, &mut recovered).map_err(|e| e.to_string());
                let _ = reply.send(result);
                break;
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_is_bounded_and_restart_never_reexports_pruned_runs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let conn = StatsManager::connect_path(&db).unwrap();
        for index in 0..12 {
            let id = format!("rotation-{index}");
            let payload =
                serde_json::json!({"schema_version":1,"run_id":id,"padding":"x".repeat(80)})
                    .to_string();
            conn.execute("INSERT INTO dictation_traces(run_id,generation,timestamp,config,outcome,payload) VALUES(?1,0,0,'{}','empty',?2)",rusqlite::params![id,payload]).unwrap();
        }
        export_pending_limit(&db, &mut false, 220).unwrap();
        let paths = files(&db).unwrap();
        assert_eq!(paths.len(), 4);
        assert!(paths.iter().all(|p| p.metadata().unwrap().len() <= 220));
        let before = paths
            .iter()
            .map(|p| (p.clone(), std::fs::read(p).unwrap()))
            .collect::<Vec<_>>();
        export_pending_limit(&db, &mut false, 220).unwrap();
        for (path, bytes) in before {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM dictation_traces WHERE exported=1",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            12
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM dictation_traces WHERE payload IS NOT NULL",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn failed_clear_publishes_epoch_and_restart_finishes_marker_before_export() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let conn = StatsManager::connect_path(&db).unwrap();
        std::fs::create_dir_all(directory(&db)).unwrap();
        let blocked = directory(&db).join("dictation-events-00000000000000000001.jsonl");
        std::fs::create_dir(&blocked).unwrap();
        let epoch = AtomicU64::new(0);
        assert!(clear(&db, &epoch).is_err());
        assert_eq!(epoch.load(Ordering::Acquire), 1);
        assert!(conn
            .query_row("SELECT clear_pending FROM telemetry_state", [], |r| r
                .get::<_, bool>(0))
            .unwrap());
        conn.execute("INSERT INTO dictation_traces(run_id,generation,timestamp,config,outcome,payload) VALUES('new',1,0,'{}','empty','{\"run_id\":\"new\"}')",[]).unwrap();
        assert!(export_pending(&db, &mut false).is_err());
        assert!(!active(&db).exists());
        std::fs::remove_dir(blocked).unwrap();
        export_pending(&db, &mut false).unwrap();
        assert_eq!(
            std::fs::read_to_string(active(&db)).unwrap(),
            "{\"run_id\":\"new\"}\n"
        );
        assert!(!conn
            .query_row("SELECT clear_pending FROM telemetry_state", [], |r| r
                .get::<_, bool>(0))
            .unwrap());
    }
}
