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
use tokio_util::sync::CancellationToken;

use crate::server::FilmcraftMcp;

/// Engine commands that block until a render is written when called with `wait: true`.
pub const LONG_COMMANDS: &[&str] = &["file.exportMedia"];

/// How often the job is polled (and at most how often progress is reported).
const POLL: Duration = Duration::from_millis(100);

/// The long calls running on one server (every clone of a [`FilmcraftMcp`] shares it), cancelled
/// ones still deleting their partial output included, and the exports its clients started in a
/// headless engine without `wait`. Shutdown cancels them first ([`Self::stop`]), waits for them,
/// and deletes what the ones it stopped wrote.
#[derive(Default)]
pub(crate) struct LongCalls {
    running: Mutex<Running>,
    next: AtomicU64,
}

#[derive(Default)]
struct Running {
    calls: HashMap<u64, Call>,
    /// Exports started without `wait` that had not finished when last looked at.
    background: Vec<Background>,
    /// The server is stopping: a call that begins now is cancelled from the start.
    stopping: bool,
}

/// An export a client started without `wait`: a job of the headless engine that goes on after the
/// call returned `{job, path}`, and would be cut off mid-file when the process exits.
struct Background {
    job: filmcraft_engine::Job,
    output: Output,
    /// Cancelled by the server stopping (else it finished before, and is left alone).
    stopped: bool,
}

/// Whether an engine job has finished (its result is in).
fn job_finished(job: &filmcraft_engine::Job) -> bool {
    job.result.lock().unwrap_or_else(PoisonError::into_inner).is_some()
}

/// Whether an engine job has finished and succeeded.
fn job_succeeded(job: &filmcraft_engine::Job) -> bool {
    job.result.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|r| r.is_ok())
}

/// A running long call: cancelled when the server stops or its session closes, and the file its
/// export writes.
struct Call {
    cancel: CancellationToken,
    output: Option<Output>,
    /// The HTTP session (`Mcp-Session-Id`) it came in, if any.
    session: Option<String>,
}

/// The file a running export writes, and since when.
struct Output {
    path: String,
    since: SystemTime,
}

impl LongCalls {
    /// How many long calls are running.
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.lock().calls.len()
    }

    fn lock(&self) -> MutexGuard<'_, Running> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one long call, of HTTP session `session` if any, for as long as the returned guard
    /// lives.
    fn enter(self: &Arc<Self>, session: Option<String>) -> LongCall {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let mut running = self.lock();
        if running.stopping {
            cancel.cancel();
        }
        running.calls.insert(id, Call { cancel: cancel.clone(), output: None, session });
        LongCall { calls: self.clone(), id, cancel }
    }

    /// HTTP session `id` is closing (its client sent `DELETE`, or the server stops): cancel its
    /// long calls now. rmcp would cancel them only once its wait for their responses is over, up
    /// to 5 s later, and meanwhile take none of their progress notifications.
    pub(crate) fn cancel_session(&self, id: &str) {
        for c in self.lock().calls.values().filter(|c| c.session.as_deref() == Some(id)) {
            c.cancel.cancel();
        }
    }

    /// A client started `job`, an export to `path`, without `wait` (since `since`): a stop
    /// cancels it if it still runs then, and deletes what it wrote. Once the server is stopping,
    /// one that begins is cancelled at once.
    pub(crate) fn background(&self, job: filmcraft_engine::Job, path: &str, since: SystemTime) {
        let mut running = self.lock();
        // the finished ones need nothing more
        running.background.retain(|b| !job_finished(&b.job));
        if job_finished(&job) {
            return;
        }
        let stopped = running.stopping;
        if stopped {
            job.progress.cancel.store(true, Ordering::Relaxed);
        }
        running.background.push(Background { job, output: Output { path: path.to_string(), since }, stopped });
    }

    /// The server is stopping: cancel every long call now, and those that begin until
    /// [`Self::drain`] ends. Session calls would otherwise learn of it only once rmcp has closed
    /// their session, after waiting up to 5 s for their responses. Exports started without `wait`
    /// that still run are cancelled too.
    pub(crate) fn stop(&self) {
        let mut running = self.lock();
        running.stopping = true;
        for c in running.calls.values() {
            c.cancel.cancel();
        }
        running.background.retain(|b| !job_finished(&b.job));
        for b in &mut running.background {
            b.job.progress.cancel.store(true, Ordering::Relaxed);
            b.stopped = true;
        }
    }

    /// Wait until no long call runs and every export the stop cancelled has ended, or `deadline`
    /// passes. Then delete the partial output of the calls still running and of the exports the
    /// stop cut short: the process is about to exit, and an export it stops leaves a truncated
    /// file. The calls must have been cancelled already ([`Self::stop`]).
    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) {
        let busy = || {
            let running = self.lock();
            !running.calls.is_empty() || running.background.iter().any(|b| b.stopped && !job_finished(&b.job))
        };
        while busy() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let left: Vec<Output> = {
            let mut running = self.lock();
            running.stopping = false;
            let calls = running.calls.values_mut().filter_map(|c| c.output.take());
            let calls: Vec<Output> = calls.collect();
            // an export that finished anyway (its last frames) is whole: kept
            let cut = std::mem::take(&mut running.background).into_iter().filter(|b| b.stopped && !job_succeeded(&b.job)).map(|b| b.output);
            calls.into_iter().chain(cut).collect()
        };
        for o in left {
            remove_partial(&o.path, o.since);
        }
    }
}

/// One running long call; dropping it ends it.
struct LongCall {
    calls: Arc<LongCalls>,
    id: u64,
    /// Cancelled when the server stops or the call's session closes.
    cancel: CancellationToken,
}

impl LongCall {
    /// The call's export writes `path`, from `since` on.
    fn writes(&self, path: &str, since: SystemTime) {
        if let Some(c) = self.calls.lock().calls.get_mut(&self.id) {
            c.output = Some(Output { path: path.to_string(), since });
        }
    }
}

impl Drop for LongCall {
    fn drop(&mut self) {
        self.calls.lock().calls.remove(&self.id);
    }
}

/// Until either token is cancelled.
async fn either_cancelled(a: &CancellationToken, b: &CancellationToken) {
    tokio::select! {
        _ = a.cancelled() => {}
        _ = b.cancelled() => {}
    }
}

/// The HTTP session (`Mcp-Session-Id`) a request came in, if any (rmcp hands the handler the
/// request's HTTP head).
fn session_of(context: &RequestContext<RoleServer>) -> Option<String> {
    let head = context.extensions.get::<http::request::Parts>()?;
    head.headers.get("mcp-session-id")?.to_str().ok().map(str::to_string)
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
    /// honouring cancellation (by the client, by the server stopping, or by the call's session
    /// closing).
    ///
    /// Every wait here also ends when the call is cancelled: the engine may be busy with another
    /// client's call for a while, and once its session is closed rmcp no longer takes the
    /// progress notifications this call sends (each would wait until rmcp's 5 s drain for the
    /// session is over), while the export goes on writing. In a headless engine the job is then
    /// stopped through its own handle, without waiting for the engine's lock either.
    pub(crate) async fn run_long(&self, id: &str, mut params: Value, context: &RequestContext<RoleServer>) -> CallToolResult {
        let call = self.long_calls.enter(session_of(context));
        let started = SystemTime::now();
        if let Some(p) = params.as_object_mut() {
            p.insert("wait".into(), json!(false));
        }
        // the engine may be busy for a while: a call cancelled meanwhile starts no job, which would
        // write its file with nobody left to stop it
        let cancelled = {
            let (by_client, by_stop) = (context.ct.clone(), call.cancel.clone());
            move || by_client.is_cancelled() || by_stop.is_cancelled()
        };
        let (start, handle) = match self.start_job_unless(id, params, cancelled).await {
            Ok(v) => v,
            Err(e) => return CallToolResult::error(vec![Content::text(e.to_string())]),
        };
        let Some(job) = start.get("job").and_then(Value::as_u64) else {
            return CallToolResult::error(vec![Content::text(format!("{id} started no job: {start}"))]);
        };
        let path = start.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        call.writes(&path, started);
        let mut rep = Reporter { peer: context.peer.clone(), token: context.meta.get_progress_token(), last: 0.0 };
        let stopped = || either_cancelled(&context.ct, &call.cancel);
        loop {
            let state = tokio::select! {
                s = self.job_state_of(job, handle.as_ref()) => s,
                _ = stopped() => break,
            };
            let finished = state.as_ref().is_none_or(is_finished);
            if let Some(j) = &state {
                let (done, total) = (j["done"].as_f64().unwrap_or(0.0), j["total"].as_f64().unwrap_or(0.0));
                tokio::select! {
                    _ = rep.report(done, total, j["status"].as_str().unwrap_or_default()) => {}
                    // a finished export's result is returned, and its file kept
                    _ = stopped() => if !finished { break },
                }
            }
            if finished {
                // the full entry (`offlineMedia` too), unless the engine stays busy past a stop
                let state = if handle.is_some() && state.is_some() {
                    tokio::select! {
                        s = self.job_state(job) => s.or(state),
                        _ = stopped() => state,
                    }
                } else {
                    state
                };
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
                _ = stopped() => break,
                _ = tokio::time::sleep(POLL) => {}
            }
        }
        // Cancelled: stop the encode at the next batch, wait for the worker, delete the partial file.
        match &handle {
            Some(j) => j.progress.cancel.store(true, Ordering::Relaxed),
            None => {
                let _ = self.run("jobs.cancel", json!({"job": job})).await;
            }
        }
        for _ in 0..600 {
            if self.job_state_of(job, handle.as_ref()).await.as_ref().is_none_or(is_finished) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        remove_partial(&path, started);
        CallToolResult::error(vec![Content::text("cancelled")])
    }

    /// The job's state: from its handle in a headless engine (without the engine's lock, and
    /// without `offlineMedia`), else its `jobs.list` entry.
    async fn job_state_of(&self, job: u64, handle: Option<&filmcraft_engine::Job>) -> Option<Value> {
        match handle {
            Some(j) => Some(j.to_json()),
            None => self.job_state(job).await,
        }
    }

    /// The job's `jobs.list` entry.
    async fn job_state(&self, job: u64) -> Option<Value> {
        let jobs = self.run("jobs.list", json!({})).await.ok()?;
        jobs.as_array()?.iter().find(|j| j["id"].as_u64() == Some(job)).cloned()
    }
}

/// Whether a job's state says it has finished.
fn is_finished(j: &Value) -> bool {
    j["finished"].as_bool() == Some(true)
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
        let call = a.enter(None);
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

    /// Stopping the server cancels the long calls running then, and those that begin until the
    /// drain is over; a later server on the same handler starts afresh.
    #[tokio::test]
    async fn stop_cancels_running_calls_and_new_ones_until_drained() {
        let calls = Arc::new(LongCalls::default());
        let running = calls.enter(None);
        assert!(!running.cancel.is_cancelled());
        calls.stop();
        assert!(running.cancel.is_cancelled(), "a running call is cancelled");
        let late = calls.enter(None);
        assert!(late.cancel.is_cancelled(), "so is one that begins while the server stops");
        drop((running, late));
        calls.drain(tokio::time::Instant::now()).await;
        assert!(!calls.enter(None).cancel.is_cancelled(), "the next server's calls run");
    }

    /// An export started without `wait` that still runs at the stop is cancelled, waited for and
    /// its output deleted; one that begins while the server stops is cancelled at once.
    #[tokio::test]
    async fn exports_started_without_wait_are_stopped_and_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("fc-long-background-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let job = |id| filmcraft_engine::Job { id, label: "export".into(), progress: Default::default(), result: Default::default() };
        let calls = Arc::new(LongCalls::default());
        let (running, late) = (job(1), job(2));
        let out = dir.join("running.mp4");
        std::fs::write(&out, b"partial").unwrap();
        calls.background(running.clone(), &out.to_string_lossy(), SystemTime::now());
        calls.stop();
        assert!(running.progress.cancel.load(Ordering::Relaxed), "cancelled by the stop");
        calls.background(late.clone(), &dir.join("late.mp4").to_string_lossy(), SystemTime::now());
        assert!(late.progress.cancel.load(Ordering::Relaxed), "cancelled as it begins");
        // the workers stop a little later
        for j in [running, late] {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                *j.result.lock().unwrap() = Some(Err("cancelled".into()));
            });
        }
        let t0 = std::time::Instant::now();
        calls.drain(tokio::time::Instant::now() + Duration::from_secs(10)).await;
        assert!(t0.elapsed() >= Duration::from_millis(150), "waited for the export to stop writing");
        assert!(!out.exists(), "its partial output is deleted");
        assert!(calls.lock().background.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing an HTTP session (its client's `DELETE`) cancels that session's long calls, and no
    /// others.
    #[test]
    fn closing_a_session_cancels_only_its_calls() {
        let calls = Arc::new(LongCalls::default());
        let (a, b, stdio) = (calls.enter(Some("a".into())), calls.enter(Some("b".into())), calls.enter(None));
        calls.cancel_session("a");
        assert!(a.cancel.is_cancelled());
        assert!(!b.cancel.is_cancelled() && !stdio.cancel.is_cancelled());
        calls.cancel_session("unknown");
        assert!(!b.cancel.is_cancelled());
        assert_eq!(calls.count(), 3, "cancelled calls count until they end");
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
