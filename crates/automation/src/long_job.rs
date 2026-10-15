//! Long exports over MCP (docs/agents.md § Long exports): `file.exportMedia` with `wait: true`
//! runs as a background engine job; the server reports its progress as MCP
//! `notifications/progress` (when the request carried a `progressToken`) and stops it on
//! `notifications/cancelled`, deleting the partial output. The session lock is only held for
//! short polls, so other requests are answered while the export runs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use rmcp::RoleServer;
use rmcp::model::{CallToolResult, ContentBlock as Content, ProgressNotificationParam, ProgressToken};
use rmcp::service::{Peer, RequestContext};
use serde_json::{Value, json};

use crate::server::FilmcraftMcp;

/// Engine commands that block until a render is written when called with `wait: true`.
pub const LONG_COMMANDS: &[&str] = &["file.exportMedia"];

/// How often the job is polled (and at most how often progress is reported).
const POLL: Duration = Duration::from_millis(100);

/// The long calls running on one server (every clone of a [`FilmcraftMcp`] shares it), cancelled
/// ones still deleting their partial output included. Shutdown waits for them, and deletes what
/// the ones still running at its deadline wrote.
#[derive(Default)]
pub(crate) struct LongCalls {
    running: Mutex<HashMap<u64, Option<Output>>>,
    next: AtomicU64,
}

/// The file a running export writes, and since when.
struct Output {
    path: String,
    since: SystemTime,
}

impl LongCalls {
    /// How many long calls are running.
    pub(crate) fn count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, Option<Output>>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one long call for as long as the returned guard lives.
    fn enter(self: &Arc<Self>) -> LongCall {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, None);
        LongCall { calls: self.clone(), id }
    }

    /// Wait until no long call runs or `deadline` passes, then delete the partial output of those
    /// still running: the process is about to exit, and an export it stops leaves a truncated file.
    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) {
        while self.count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let left: Vec<Output> = self.lock().values_mut().filter_map(Option::take).collect();
        for o in left {
            remove_partial(&o.path, o.since);
        }
    }
}

/// One running long call; dropping it ends it.
struct LongCall {
    calls: Arc<LongCalls>,
    id: u64,
}

impl LongCall {
    /// The call's export writes `path`, from `since` on.
    fn writes(&self, path: &str, since: SystemTime) {
        if let Some(o) = self.calls.lock().get_mut(&self.id) {
            *o = Some(Output { path: path.to_string(), since });
        }
    }
}

impl Drop for LongCall {
    fn drop(&mut self) {
        self.calls.lock().remove(&self.id);
    }
}

/// Whether `command_run {id, params}` is a long call this module runs.
pub fn is_long(id: &str, params: &Value) -> bool {
    LONG_COMMANDS.contains(&id) && params.get("wait").and_then(Value::as_bool) == Some(true)
}

/// Sends `notifications/progress` for one request; progress only ever increases.
struct Reporter {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
    last: f64,
}

impl Reporter {
    async fn report(&mut self, done: f64, total: f64, message: &str) {
        let Some(token) = self.token.clone() else { return };
        if total <= 0.0 || done <= self.last {
            return;
        }
        self.last = done;
        let mut p = ProgressNotificationParam::new(token, done).with_total(total);
        if !message.is_empty() {
            p = p.with_message(message);
        }
        let _ = self.peer.notify_progress(p).await;
    }
}

impl FilmcraftMcp {
    /// Run `id` (a [`LONG_COMMANDS`] entry, `wait: true`) as a job, reporting progress and
    /// honouring cancellation.
    pub(crate) async fn run_long(&self, id: &str, mut params: Value, context: &RequestContext<RoleServer>) -> CallToolResult {
        let call = self.long_calls.enter();
        let started = SystemTime::now();
        if let Some(p) = params.as_object_mut() {
            p.insert("wait".into(), json!(false));
        }
        let start = match self.run(id, params).await {
            Ok(v) => v,
            Err(e) => return CallToolResult::error(vec![Content::text(e.to_string())]),
        };
        let Some(job) = start.get("job").and_then(Value::as_u64) else {
            return CallToolResult::error(vec![Content::text(format!("{id} started no job: {start}"))]);
        };
        let path = start.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        call.writes(&path, started);
        let mut rep = Reporter { peer: context.peer.clone(), token: context.meta.get_progress_token(), last: 0.0 };
        loop {
            let state = self.job_state(job).await;
            let finished = state.as_ref().is_none_or(|j| j["finished"].as_bool() == Some(true));
            if let Some(j) = &state {
                let (done, total) = (j["done"].as_f64().unwrap_or(0.0), j["total"].as_f64().unwrap_or(0.0));
                rep.report(done, total, j["status"].as_str().unwrap_or_default()).await;
            }
            if finished {
                let offline = state.as_ref().and_then(|j| j.get("offlineMedia").cloned());
                let result = state.map(|j| j["result"].clone()).unwrap_or(Value::Null);
                if let Some(e) = result.get("error").and_then(Value::as_str) {
                    return CallToolResult::error(vec![Content::text(format!("export failed: {e}"))]);
                }
                let mut out = start;
                if let Some(o) = out.as_object_mut() {
                    o.insert("result".into(), result);
                    if let Some(off) = offline {
                        o.insert("offlineMedia".into(), off);
                    }
                }
                return CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&out).unwrap_or_default())]);
            }
            tokio::select! {
                _ = context.ct.cancelled() => break,
                _ = tokio::time::sleep(POLL) => {}
            }
        }
        // Cancelled: stop the encode at the next batch, wait for the worker, delete the partial file.
        let _ = self.run("jobs.cancel", json!({"job": job})).await;
        for _ in 0..600 {
            if self.job_state(job).await.is_none_or(|j| j["finished"].as_bool() == Some(true)) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        remove_partial(&path, started);
        CallToolResult::error(vec![Content::text("cancelled")])
    }

    /// The job's `jobs.list` entry.
    async fn job_state(&self, job: u64) -> Option<Value> {
        let jobs = self.run("jobs.list", json!({})).await.ok()?;
        jobs.as_array()?.iter().find(|j| j["id"].as_u64() == Some(job)).cloned()
    }
}

/// Delete what an interrupted export wrote, and nothing else: the output file itself, the numbered
/// stills of an image sequence (`<stem><3+ digits>.<same extension>`, only when the output is a
/// PNG/TIFF/BMP/JPEG/Targa/DPX sequence) and the caption sidecar
/// (`<stem>.srt` / `<stem>.vtt`), each only if it was written since `since`. Other files that
/// merely share the name's stem (`<stem>.aep`, `<stem>2.psd`, `<stem>1.mp4` beside a movie, …) are
/// never touched, even when they were saved during the export.
pub fn remove_partial(path: &str, since: SystemTime) {
    let p = Path::new(path);
    let (Some(dir), Some(stem), Some(file)) = (p.parent(), p.file_stem().and_then(|s| s.to_str()), p.file_name().and_then(|s| s.to_str())) else {
        return;
    };
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or_default();
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    // file times are coarser than the clock
    let since = since.checked_sub(Duration::from_secs(1)).unwrap_or(since);
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_export_output(name, file, stem, ext) && e.metadata().is_ok_and(|m| m.is_file() && m.modified().is_ok_and(|t| t >= since)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Whether `name` is a file an export to `file` (`stem` + `.ext`) writes.
fn is_export_output(name: &str, file: &str, stem: &str, ext: &str) -> bool {
    if name == file {
        return true;
    }
    let Some(rest) = name.strip_prefix(stem) else { return false };
    if let Some(sidecar) = rest.strip_prefix('.') {
        return sidecar.eq_ignore_ascii_case("srt") || sidecar.eq_ignore_ascii_case("vtt");
    }
    // image sequence: the frame number (at least three digits, like `image_sequence_path`), then the
    // output's own extension; movies and audio are a single file, so numbered neighbours are not theirs
    if !is_sequence_extension(ext) {
        return false;
    }
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    digits >= 3 && rest.get(digits..).is_some_and(|r| r.strip_prefix('.').is_some_and(|x| x == ext))
}

/// Whether an export to a file with extension `ext` writes an image sequence (one still per frame).
fn is_sequence_extension(ext: &str) -> bool {
    use filmcraft_engine::export::Format;
    Format::from_name(ext).is_some_and(Format::is_image_sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exports_own_files_are_removed() {
        let dir = std::env::temp_dir().join(format!("fc-remove-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now();
        let ours = ["trailer.png", "trailer000.png", "trailer123.png", "trailer.srt", "trailer.vtt"];
        // same stem, written during the export, but not the export's
        let theirs = ["trailer.aep", "trailer2.psd", "trailer.docx", "trailer000.jpg", "trailers.png", "trailer.png.bak", "other.png"];
        for f in ours.iter().chain(theirs.iter()) {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        remove_partial(&dir.join("trailer.png").to_string_lossy(), started);
        for f in ours {
            assert!(!dir.join(f).exists(), "{f} should be removed");
        }
        for f in theirs {
            assert!(dir.join(f).exists(), "{f} must not be removed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbered_files_beside_a_movie_are_kept() {
        let dir = std::env::temp_dir().join(format!("fc-remove-partial-movie-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now();
        // a movie export writes one file: `clip1.mp4` and `clip002.mp4` are other exports
        let ours = ["clip.mp4", "clip.srt"];
        let theirs = ["clip1.mp4", "clip002.mp4", "clip2.mov"];
        for f in ours.iter().chain(theirs.iter()) {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        remove_partial(&dir.join("clip.mp4").to_string_lossy(), started);
        for f in ours {
            assert!(!dir.join(f).exists(), "{f} should be removed");
        }
        for f in theirs {
            assert!(dir.join(f).exists(), "{f} must not be removed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sequence_frames_have_at_least_three_digits() {
        let dir = std::env::temp_dir().join(format!("fc-remove-partial-digits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now();
        // frames are numbered with at least three digits (`shot000.tiff`); `shot2.tiff` is not one
        let ours = ["shot.tiff", "shot000.tiff", "shot1234.tiff"];
        let theirs = ["shot2.tiff", "shot12.tiff"];
        for f in ours.iter().chain(theirs.iter()) {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        remove_partial(&dir.join("shot.tiff").to_string_lossy(), started);
        for f in ours {
            assert!(!dir.join(f).exists(), "{f} should be removed");
        }
        for f in theirs {
            assert!(dir.join(f).exists(), "{f} must not be removed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shutdown deletes what an export still running at its deadline wrote (the process exits
    /// before the export's own cancellation could), and each server counts only its own calls.
    #[tokio::test]
    async fn drain_deletes_the_output_of_calls_still_running() {
        let dir = std::env::temp_dir().join(format!("fc-long-calls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (Arc::new(LongCalls::default()), Arc::new(LongCalls::default()));
        let started = SystemTime::now();
        let call = a.enter();
        let path = dir.join("out.mp4");
        std::fs::write(&path, b"partial").unwrap();
        std::fs::write(dir.join("other.mp4"), b"x").unwrap();
        call.writes(&path.to_string_lossy(), started);
        assert_eq!((a.count(), b.count()), (1, 0));
        let t0 = std::time::Instant::now();
        b.drain(tokio::time::Instant::now() + Duration::from_secs(10)).await;
        assert!(t0.elapsed() < Duration::from_secs(1), "nothing runs on b: no wait");
        assert!(path.exists(), "b's drain leaves a's export alone");
        a.drain(tokio::time::Instant::now() + Duration::from_millis(200)).await;
        assert!(!path.exists(), "the partial file of a call still running is deleted");
        assert!(dir.join("other.mp4").exists());
        drop(call);
        assert_eq!(a.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_older_than_the_export_are_kept() {
        let dir = std::env::temp_dir().join(format!("fc-remove-partial-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        remove_partial(&dir.join("clip.mp4").to_string_lossy(), SystemTime::now() + Duration::from_secs(60));
        assert!(dir.join("clip.mp4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
