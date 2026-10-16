//! Long exports over MCP (docs/agents.md § Long exports): `file.exportMedia` or `export.quick`
//! with `wait: true`, from `command_run` or a `command_batch` step, runs as a background engine
//! job; the server
//! reports its progress as MCP `notifications/progress` (when the request carried a
//! `progressToken`) and stops it on `notifications/cancelled`, deleting the partial output. The
//! session lock is only held for short polls, so other requests are answered while the export
//! runs.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use rmcp::RoleServer;
use rmcp::model::{CallToolResult, ContentBlock as Content, ProgressNotificationParam, ProgressToken};
use rmcp::service::{Peer, RequestContext};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::server::FilmcraftMcp;

/// Engine commands that block until a render is written when called with `wait: true`, and that
/// return `{job, path, ...}` at once without it (their export goes on as a job).
pub const LONG_COMMANDS: &[&str] = &["file.exportMedia", "export.quick"];

/// How often the job is polled (and at most how often progress is reported).
const POLL: Duration = Duration::from_millis(100);

/// How many closed sessions [`LongCalls`] remembers (see [`LongCalls::cancel_session`]).
const CLOSED_KEPT: usize = 1024;

/// The long calls running on one server (every clone of a [`FilmcraftMcp`] shares it), cancelled
/// ones still deleting their partial output included, and the exports its clients started in a
/// headless engine without `wait`. Shutdown cancels them first ([`Self::stop`]), waits for those
/// with an export to clean up, and deletes what the ones it stopped wrote.
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
    /// Only its exports are stopping (a stdio client closed its input): an export that begins now
    /// is cancelled from the start; other calls, and a batch's other steps, run.
    exports_stopping: bool,
    /// The sessions closed last (at most [`CLOSED_KEPT`], newest last): a call of one of them
    /// that begins now is cancelled from the start.
    closed: VecDeque<String>,
}

/// An export a client started without `wait`: a job of the headless engine that goes on after the
/// call returned `{job, path}`, and would be cut off mid-file when the process exits.
struct Background {
    job: filmcraft_engine::Job,
    output: Output,
    /// Cancelled by the server stopping (else it finished before, and is left alone).
    stopped: bool,
}

impl Background {
    /// Whether what it wrote must go: the stop cut it short. One that finished anyway (its last
    /// frames written before the stop reached it) is whole, and kept.
    fn cut(&self) -> bool {
        self.stopped && !job_succeeded(&self.job)
    }
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
    /// What its exports watch: cancelled with `cancel`, and also when only exports stop
    /// ([`LongCalls::stop_exports`]).
    export: CancellationToken,
    /// It runs an export now (a long call, or a batch while a step of it exports): a stop waits for
    /// it to delete what that wrote. Another step of a batch has nothing to delete, and may hold the
    /// engine for long (`export.queue.start` with `wait`, which nothing cancels): never waited for.
    exporting: bool,
    output: Option<Output>,
    /// The HTTP session (`Mcp-Session-Id`) it came in, if any.
    session: Option<String>,
}

/// The file a running export writes, and since when; in a headless engine, its job.
struct Output {
    path: String,
    since: SystemTime,
    job: Option<filmcraft_engine::Job>,
}

impl Output {
    /// Whether what it wrote must go: an export that finished anyway is whole, and kept.
    fn cut(&self) -> bool {
        !self.job.as_ref().is_some_and(job_succeeded)
    }
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

    /// Whether the server, or only its exports, are stopping: long calls (or exports) that begin
    /// now are cancelled from the start.
    #[cfg(test)]
    pub(crate) fn is_stopping(&self) -> bool {
        let running = self.lock();
        running.stopping || running.exports_stopping
    }

    /// Counts one long call, of HTTP session `session` if any, for as long as the returned guard
    /// lives; `exporting` when it is an export itself (else a batch, which exports only in some of
    /// its steps: [`LongCall::exports`]). It is cancelled from the start when the server is stopping
    /// or its session was closed already, and its exports are when only those are stopping.
    fn enter(self: &Arc<Self>, session: Option<String>, exporting: bool) -> LongCall {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let export = cancel.child_token();
        let mut running = self.lock();
        if running.stopping || session.as_ref().is_some_and(|s| running.closed.contains(s)) {
            cancel.cancel();
        }
        if running.exports_stopping {
            export.cancel();
        }
        running.calls.insert(id, Call { cancel: cancel.clone(), export: export.clone(), exporting, output: None, session });
        LongCall { calls: self.clone(), id, cancel, export }
    }

    /// HTTP session `id` is closing (its client sent `DELETE`, or the server stops): cancel its
    /// long calls now. rmcp would cancel them only once its wait for their responses is over, up
    /// to 5 s later, and meanwhile take none of their progress notifications.
    ///
    /// A call rmcp had already taken from that session, but whose handler has not yet begun, is
    /// cancelled when it begins ([`Self::enter`]): it would otherwise start its export, and encode
    /// until rmcp's wait was over, with nobody tracking its session any more.
    pub(crate) fn cancel_session(&self, id: &str) {
        let mut running = self.lock();
        for c in running.calls.values().filter(|c| c.session.as_deref() == Some(id)) {
            c.cancel.cancel();
        }
        if !running.closed.iter().any(|c| c == id) {
            if running.closed.len() >= CLOSED_KEPT {
                running.closed.pop_front();
            }
            running.closed.push_back(id.to_string());
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
        let stopped = running.stopping || running.exports_stopping;
        if stopped {
            job.progress.cancel.store(true, Ordering::Relaxed);
        }
        running.background.push(Background { job, output: Output { path: path.to_string(), since, job: None }, stopped });
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
        stop_background(&mut running);
    }

    /// A stdio client closed its input: cancel every export now (long calls, the steps of batches
    /// that export, exports started without `wait`), and those that begin until [`Self::drain`]
    /// ends, as a stop does. Other calls, and the other steps of a batch, run on as rmcp lets them
    /// (it waits up to 5 s for their responses), as `command_run`s sent before the input closed do.
    pub(crate) fn stop_exports(&self) {
        let mut running = self.lock();
        running.exports_stopping = true;
        for c in running.calls.values() {
            c.export.cancel();
        }
        stop_background(&mut running);
    }

    /// Wait until no call has an export to clean up and every export the stop cancelled has ended,
    /// or `deadline` passes; what an export the stop cut short wrote is deleted as soon as it has
    /// ended, so that a supervisor that kills the process before the grace is over (docker stop
    /// after 10 s, a second Ctrl+C) finds no partial file. Then delete the partial output of the
    /// exports still running: the process is about to exit, and an export it stops leaves a
    /// truncated file. One that finished anyway, its last frames written before the stop reached
    /// it, is whole: kept. The calls must have been cancelled already ([`Self::stop`]).
    ///
    /// A batch is waited for only while a step of it exports: another step may hold the engine for
    /// long (`export.queue.start` with `wait`), and has nothing to delete. It used to keep the stop
    /// waiting for its whole grace, as the same command from `command_run` never did.
    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) {
        loop {
            let (busy, ended) = {
                let mut running = self.lock();
                let (ended, going): (Vec<Background>, Vec<Background>) =
                    std::mem::take(&mut running.background).into_iter().partition(|b| job_finished(&b.job));
                running.background = going;
                let busy = running.calls.values().any(|c| c.exporting) || running.background.iter().any(|b| b.stopped);
                (busy, ended)
            };
            for b in ended.iter().filter(|b| b.cut()) {
                remove_partial(&b.output.path, b.output.since);
            }
            if !busy || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let left: Vec<Output> = {
            let mut running = self.lock();
            running.stopping = false;
            running.exports_stopping = false;
            let calls = running.calls.values_mut().filter_map(|c| c.output.take()).filter(Output::cut);
            let calls: Vec<Output> = calls.collect();
            // an export that finished anyway (its last frames) is whole: kept
            let cut = std::mem::take(&mut running.background).into_iter().filter(Background::cut).map(|b| b.output);
            calls.into_iter().chain(cut).collect()
        };
        for o in left {
            remove_partial(&o.path, o.since);
        }
    }
}

/// Cancel the exports started without `wait` that still run, and forget those that have ended.
fn stop_background(running: &mut Running) {
    running.background.retain(|b| !job_finished(&b.job));
    for b in &mut running.background {
        b.job.progress.cancel.store(true, Ordering::Relaxed);
        b.stopped = true;
    }
}

/// One running long call (or `command_batch`); dropping it ends it.
pub(crate) struct LongCall {
    calls: Arc<LongCalls>,
    id: u64,
    /// Cancelled when the server stops or the call's session closes.
    cancel: CancellationToken,
    /// What its exports watch: cancelled with `cancel`, and when only exports stop (a stdio client
    /// closed its input).
    export: CancellationToken,
}

impl LongCall {
    /// Whether the call is cancelled: by its client (`ct`, its request's token), by the server
    /// stopping or by its session closing.
    pub(crate) fn cancelled(&self, ct: &CancellationToken) -> bool {
        ct.is_cancelled() || self.cancel.is_cancelled()
    }

    /// The call's export writes `path`, from `since` on, as `job` (in a headless engine).
    fn writes(&self, path: &str, since: SystemTime, job: Option<&filmcraft_engine::Job>) {
        if let Some(c) = self.calls.lock().calls.get_mut(&self.id) {
            c.output = Some(Output { path: path.to_string(), since, job: job.cloned() });
        }
    }

    /// A step of this batch exports, until [`Self::kept`]: a stop waits for it to delete what it
    /// wrote, as for a long call of its own.
    pub(crate) fn exports(&self) {
        if let Some(c) = self.calls.lock().calls.get_mut(&self.id) {
            c.exporting = true;
        }
    }

    /// The call's export finished and is whole (or is over, for a step of a batch): its file is
    /// never deleted, and a stop no longer waits for it.
    pub(crate) fn kept(&self) {
        if let Some(c) = self.calls.lock().calls.get_mut(&self.id) {
            c.output = None;
            c.exporting = false;
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

/// Whether `command_run {id, params}`, or a `command_batch` step, is a long call this module runs.
pub fn is_long(id: &str, params: &Value) -> bool {
    LONG_COMMANDS.contains(&id) && params.get("wait").and_then(Value::as_bool) == Some(true)
}

/// Sends `notifications/progress` for one request; progress only ever increases, also across the
/// long steps of one `command_batch` ([`Reporter::next_step`]).
pub(crate) struct Reporter {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
    last: f64,
    /// Where this step's progress starts: what the earlier steps of a batch reported.
    base: f64,
}

impl Reporter {
    /// For the request of `context`: reports only when it carries a `progressToken`.
    pub(crate) fn new(context: &RequestContext<RoleServer>) -> Self {
        Self { peer: context.peer.clone(), token: context.meta.get_progress_token(), last: 0.0, base: 0.0 }
    }

    async fn report(&mut self, done: f64, total: f64, message: &str) {
        let Some(token) = self.token.clone() else { return };
        let (done, total) = (self.base + done, self.base + total);
        if total <= self.base || done <= self.last {
            return;
        }
        self.last = done;
        let mut p = ProgressNotificationParam::new(token, done).with_total(total);
        if !message.is_empty() {
            p = p.with_message(message);
        }
        let _ = self.peer.notify_progress(p).await;
    }

    /// The next long step of a batch reports on from where this one got to.
    pub(crate) fn next_step(&mut self) {
        self.base = self.last;
    }
}

/// Why a long call returned no result.
#[derive(Debug, PartialEq)]
pub(crate) enum LongError {
    /// Cancelled (by its client, by the server stopping, by its session closing, or, an export, by
    /// a stdio client closing its input) before its export finished: it started none, or what it
    /// wrote is deleted.
    Cancelled,
    /// The command failed, or its export did.
    Failed(String),
}

impl std::fmt::Display for LongError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LongError::Cancelled => f.write_str("cancelled"),
            LongError::Failed(m) => f.write_str(m),
        }
    }
}

/// Who has the start of a long call's export, between the call and the engine (see
/// [`FilmcraftMcp::long_call`]).
const WAITING: u8 = 0;
/// The engine runs the command that starts the export.
const STARTING: u8 = 1;
/// The call was cancelled while it waited for the engine, and returned: the engine must not start
/// the export.
const ABANDONED: u8 = 2;

impl FilmcraftMcp {
    /// [`Self::long_call`] as a tool result.
    pub(crate) async fn run_long(&self, id: &str, params: Value, context: &RequestContext<RoleServer>) -> CallToolResult {
        let call = self.begin_long(context, true);
        match self.long_call(&call, id, params, context, &mut Reporter::new(context)).await {
            Ok(v) => CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&v).unwrap_or_default())]),
            Err(e) => CallToolResult::error(vec![Content::text(e.to_string())]),
        }
    }

    /// Count the request of `context` as a long call (a long export, `exporting`, or a whole
    /// `command_batch`) while the returned guard lives: a stop, or its session closing, cancels it.
    pub(crate) fn begin_long(&self, context: &RequestContext<RoleServer>, exporting: bool) -> LongCall {
        self.long_calls.enter(session_of(context), exporting)
    }

    /// A `command_batch` step that is no long call, run as [`Self::run_command_unless`] unless
    /// `call` (the batch) is cancelled first: while it waits for the engine, which another client's
    /// call may keep busy, a cancel returns at once, and the engine never runs the step. One that
    /// comes just as the engine takes the step waits the moment the step takes (see `claim` in
    /// [`Self::long_call`]).
    pub(crate) async fn batch_step(&self, call: &LongCall, id: &str, params: Value, context: &RequestContext<RoleServer>) -> Result<Value, LongError> {
        let claim = Arc::new(AtomicU8::new(WAITING));
        let cancelled = {
            let (by_client, by_stop, claim) = (context.ct.clone(), call.cancel.clone(), claim.clone());
            move || {
                by_client.is_cancelled() || by_stop.is_cancelled() || claim.compare_exchange(WAITING, STARTING, Ordering::AcqRel, Ordering::Acquire).is_err()
            }
        };
        let run = self.run_command_unless(id, params, cancelled);
        tokio::pin!(run);
        let r = tokio::select! {
            r = &mut run => r,
            _ = either_cancelled(&context.ct, &call.cancel) => {
                if claim.compare_exchange(WAITING, ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return Err(LongError::Cancelled);
                }
                run.await
            }
        };
        match r {
            Ok(v) => Ok(v),
            Err(_) if call.cancelled(&context.ct) => Err(LongError::Cancelled),
            Err(e) => Err(LongError::Failed(e.to_string())),
        }
    }

    /// Run `id` (a [`LONG_COMMANDS`] entry, `wait: true`) as a job, for `call` (from
    /// [`Self::begin_long`]: this request, or the batch it is a step of), reporting progress through
    /// `rep` and honouring cancellation (by the client, by the server stopping, by the call's
    /// session closing, or by a stdio client closing its input). The result is a blocking export's:
    /// `{job, path, result, offlineMedia}`.
    ///
    /// Every wait here also ends when the call is cancelled: the engine may be busy with another
    /// client's call for a while, and once its session is closed rmcp no longer takes the
    /// progress notifications this call sends (each would wait until rmcp's 5 s drain for the
    /// session is over), while the export goes on writing. In a headless engine the job is then
    /// stopped through its own handle, without waiting for the engine's lock either.
    pub(crate) async fn long_call(
        &self,
        call: &LongCall,
        id: &str,
        mut params: Value,
        context: &RequestContext<RoleServer>,
        rep: &mut Reporter,
    ) -> Result<Value, LongError> {
        let started = SystemTime::now();
        if let Some(p) = params.as_object_mut() {
            p.insert("wait".into(), json!(false));
        }
        let stopped = || either_cancelled(&context.ct, &call.export);
        // The engine may be busy for a while (another client's export that waits, a batch): a
        // call cancelled meanwhile returns at once, instead of holding a stop for its whole
        // grace. Once the engine is free it starts no job, which would write its file with
        // nobody left to stop it. `claim` settles a cancel that comes just as the engine takes
        // the command: either the engine has not begun (the call returns, the engine starts
        // nothing), or it has (the call waits the moment it takes to start, and stops the job).
        let claim = Arc::new(AtomicU8::new(WAITING));
        let cancelled = {
            let (by_client, by_stop, claim) = (context.ct.clone(), call.export.clone(), claim.clone());
            move || {
                by_client.is_cancelled() || by_stop.is_cancelled() || claim.compare_exchange(WAITING, STARTING, Ordering::AcqRel, Ordering::Acquire).is_err()
            }
        };
        let start = self.start_job_unless(id, params, cancelled);
        tokio::pin!(start);
        let begun = tokio::select! {
            r = &mut start => r,
            _ = stopped() => {
                if claim.compare_exchange(WAITING, ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return Err(LongError::Cancelled);
                }
                // the engine is starting it right now: take the job, to stop it
                start.await
            }
        };
        let (start, handle) = match begun {
            Ok(v) => v,
            Err(_) if context.ct.is_cancelled() || call.export.is_cancelled() => return Err(LongError::Cancelled),
            Err(e) => return Err(LongError::Failed(e.to_string())),
        };
        let Some(job) = start.get("job").and_then(Value::as_u64) else {
            return Err(LongError::Failed(format!("{id} started no job: {start}")));
        };
        let path = start.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        call.writes(&path, started, handle.as_ref());
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
                if ended_whole(state.as_ref()) {
                    call.kept();
                }
                return finished_result(start, state);
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
        let mut last = None;
        for _ in 0..600 {
            last = self.job_state_of(job, handle.as_ref()).await;
            if last.as_ref().is_none_or(is_finished) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        // It may have finished just before the cancel reached it (its last batch written while this
        // call waited to poll again): then the file is whole, and deleting it would lose a finished
        // export. Its result is returned (a client that cancelled ignores it).
        if ended_whole(last.as_ref()) {
            call.kept();
            return finished_result(start, last);
        }
        remove_partial(&path, started);
        Err(LongError::Cancelled)
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

/// A finished export's call result: `start` (`{job, path}`) with the job's `result` and
/// `offlineMedia` from `state`, its job entry; an error when the export failed.
fn finished_result(start: Value, state: Option<Value>) -> Result<Value, LongError> {
    let offline = state.as_ref().and_then(|j| j.get("offlineMedia").cloned());
    let result = state.map(|j| j["result"].clone()).unwrap_or(Value::Null);
    if let Some(e) = result.get("error").and_then(Value::as_str) {
        return Err(LongError::Failed(format!("export failed: {e}")));
    }
    let mut out = start;
    if let Some(o) = out.as_object_mut() {
        o.insert("result".into(), result);
        if let Some(off) = offline {
            o.insert("offlineMedia".into(), off);
        }
    }
    Ok(out)
}

/// Whether a job's state (its `jobs.list` entry, or its handle's JSON) says its export finished
/// and succeeded: its file is whole, whether or not a cancel came meanwhile. Unknown, unfinished,
/// failed or cancelled: not whole.
fn ended_whole(state: Option<&Value>) -> bool {
    state.is_some_and(|j| is_finished(j) && j["result"].is_object() && j["result"].get("error").is_none())
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
        let dir = crate::TestDir::new("remove-partial");
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
    }

    #[test]
    fn numbered_files_beside_a_movie_are_kept() {
        let dir = crate::TestDir::new("remove-partial-movie");
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
    }

    #[test]
    fn sequence_frames_have_at_least_three_digits() {
        let dir = crate::TestDir::new("remove-partial-digits");
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
    }

    /// Shutdown deletes what an export still running at its deadline wrote (the process exits
    /// before the export's own cancellation could), and each server counts only its own calls.
    #[tokio::test]
    async fn drain_deletes_the_output_of_calls_still_running() {
        let dir = crate::TestDir::new("long-calls");
        let (a, b) = (Arc::new(LongCalls::default()), Arc::new(LongCalls::default()));
        let started = SystemTime::now();
        let call = a.enter(None, true);
        let path = dir.join("out.mp4");
        std::fs::write(&path, b"partial").unwrap();
        std::fs::write(dir.join("other.mp4"), b"x").unwrap();
        call.writes(&path.to_string_lossy(), started, None);
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
    }

    /// Stopping the server cancels the long calls running then, and those that begin until the
    /// drain is over; a later server on the same handler starts afresh.
    #[tokio::test]
    async fn stop_cancels_running_calls_and_new_ones_until_drained() {
        let calls = Arc::new(LongCalls::default());
        let running = calls.enter(None, true);
        assert!(!running.cancel.is_cancelled());
        calls.stop();
        assert!(running.cancel.is_cancelled(), "a running call is cancelled");
        let late = calls.enter(None, true);
        assert!(late.cancel.is_cancelled(), "so is one that begins while the server stops");
        drop((running, late));
        calls.drain(tokio::time::Instant::now()).await;
        assert!(!calls.enter(None, true).cancel.is_cancelled(), "the next server's calls run");
    }

    /// A stdio client closing its input stops only exports: every call's exports (a long call, the
    /// export steps of a batch) and those that begin until the drain is over, exports started
    /// without `wait` too; a batch itself goes on with its other steps. Closing the input used to
    /// stop it all, so a piped batch of edits and a save ran none of its steps.
    #[test]
    fn closing_the_input_stops_only_exports() {
        let dir = crate::TestDir::new("long-eof");
        let job = |id| filmcraft_engine::Job { id, label: "export".into(), progress: Default::default(), result: Default::default() };
        let calls = Arc::new(LongCalls::default());
        let (export, batch) = (calls.enter(None, true), calls.enter(None, false));
        let running = job(1);
        calls.background(running.clone(), &dir.join("running.mp4").to_string_lossy(), SystemTime::now());
        calls.stop_exports();
        assert!(export.export.is_cancelled() && batch.export.is_cancelled(), "their exports stop");
        assert!(!export.cancel.is_cancelled() && !batch.cancel.is_cancelled(), "the calls themselves run on");
        assert!(running.progress.cancel.load(Ordering::Relaxed), "an export without wait stops too");
        let late = calls.enter(None, false);
        assert!(late.export.is_cancelled() && !late.cancel.is_cancelled(), "a batch that begins now runs, without exports");
        let later = job(2);
        calls.background(later.clone(), &dir.join("later.mp4").to_string_lossy(), SystemTime::now());
        assert!(later.progress.cancel.load(Ordering::Relaxed));
        // a stop that follows (the server ends) stops the rest
        calls.stop();
        assert!(batch.cancel.is_cancelled() && late.cancel.is_cancelled());
        drop((export, batch, late));
        for j in [running, later] {
            *j.result.lock().unwrap() = Some(Err("cancelled".into()));
        }
        futures_util::FutureExt::now_or_never(calls.drain(tokio::time::Instant::now())).expect("nothing to wait for");
        let next = calls.enter(None, false);
        assert!(!next.cancel.is_cancelled() && !next.export.is_cancelled(), "the next server's calls run");
    }

    /// A stop waits only for the calls that have an export to clean up: a long call, or a batch
    /// while a step of it exports. A batch whose other step holds the engine (`export.queue.start`
    /// with `wait`, which nothing cancels) used to keep a stop waiting for its whole grace, though it
    /// had nothing to delete, while the same command from `command_run` was never waited for.
    #[tokio::test]
    async fn a_stop_waits_only_for_calls_with_an_export_to_clean_up() {
        let calls = Arc::new(LongCalls::default());
        let batch = calls.enter(None, false);
        calls.stop();
        let t0 = std::time::Instant::now();
        calls.drain(tokio::time::Instant::now() + Duration::from_secs(10)).await;
        assert!(t0.elapsed() < Duration::from_secs(1), "a batch outside an export step is not waited for: {:?}", t0.elapsed());
        // one in an export step is, until the step is over
        batch.exports();
        calls.stop();
        let step = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            batch.kept();
            batch
        });
        let t0 = std::time::Instant::now();
        calls.drain(tokio::time::Instant::now() + Duration::from_secs(10)).await;
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(5), "waited for the export step: {took:?}");
        let batch = step.join().unwrap();
        // and not once it is over
        calls.stop();
        let t0 = std::time::Instant::now();
        calls.drain(tokio::time::Instant::now() + Duration::from_secs(10)).await;
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        drop(batch);
    }

    /// An export started without `wait` that the stop cut short has its partial file deleted as
    /// soon as it has stopped, while the stop still waits for another export: it used to be deleted
    /// only at the end of the wait, so a supervisor that killed the process before (docker stop
    /// after 10 s, a second Ctrl+C) left it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_export_the_stop_cut_short_is_deleted_as_soon_as_it_has_stopped() {
        let dir = crate::TestDir::new("long-background-early");
        let calls = Arc::new(LongCalls::default());
        let job = filmcraft_engine::Job { id: 1, label: "export".into(), progress: Default::default(), result: Default::default() };
        let out = dir.join("background.mp4");
        std::fs::write(&out, b"partial").unwrap();
        calls.background(job.clone(), &out.to_string_lossy(), SystemTime::now());
        // another export that is slow to stop
        let slow = calls.enter(None, true);
        calls.stop();
        let drain = tokio::spawn({
            let calls = calls.clone();
            async move { calls.drain(tokio::time::Instant::now() + Duration::from_secs(20)).await }
        });
        // its worker stops a little later
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(job.progress.cancel.load(Ordering::Relaxed));
        *job.result.lock().unwrap() = Some(Err("cancelled".into()));
        let t0 = std::time::Instant::now();
        while out.exists() {
            assert!(t0.elapsed() < Duration::from_secs(3), "the partial file of the stopped export is still there");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!drain.is_finished(), "the stop still waits for the other export");
        drop(slow);
        tokio::time::timeout(Duration::from_secs(5), drain).await.expect("the wait ends with the last export").unwrap();
        assert!(calls.lock().background.is_empty());
    }

    /// An export started without `wait` that still runs at the stop is cancelled, waited for and
    /// its output deleted; one that begins while the server stops is cancelled at once.
    #[tokio::test]
    async fn exports_started_without_wait_are_stopped_and_cleaned_up() {
        let dir = crate::TestDir::new("long-background");
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
    }

    /// A call of a session that was closed before the call began (rmcp had taken the request,
    /// but its handler had not reached [`LongCalls::enter`] when the client's `DELETE` came) is
    /// cancelled from the start. It used to start its export and encode for rmcp's 5 s drain, while
    /// the gate no longer tracked its session. Closed sessions are remembered up to a bound.
    #[test]
    fn a_call_of_a_session_closed_already_is_cancelled_from_the_start() {
        let calls = Arc::new(LongCalls::default());
        calls.cancel_session("a");
        assert!(calls.enter(Some("a".into()), true).cancel.is_cancelled(), "its session was closed");
        assert!(!calls.enter(Some("b".into()), true).cancel.is_cancelled());
        assert!(!calls.enter(None, true).cancel.is_cancelled());
        calls.cancel_session("a");
        assert_eq!(calls.lock().closed.len(), 1, "remembered once");
        for i in 0..CLOSED_KEPT {
            calls.cancel_session(&format!("s{i}"));
        }
        assert_eq!(calls.lock().closed.len(), CLOSED_KEPT, "bounded");
        assert!(!calls.enter(Some("a".into()), true).cancel.is_cancelled(), "the oldest is forgotten");
        assert!(calls.enter(Some(format!("s{}", CLOSED_KEPT - 1)), true).cancel.is_cancelled());
    }

    /// A job whose export finished, report in, with `result` as `jobs.list` and its handle give it.
    fn finished_job(id: u64, result: Result<(), &str>) -> filmcraft_engine::Job {
        let job = filmcraft_engine::Job { id, label: "export".into(), progress: Default::default(), result: Default::default() };
        let report =
            filmcraft_engine::export::Report { path: "out.mp4".into(), frames: 24, seconds: 1.0, bytes: 1000, render_fps: 60.0, extra_files: Vec::new() };
        *job.result.lock().unwrap() = Some(result.map(|()| report).map_err(str::to_string));
        job
    }

    /// An export that finished just before a cancel or a stop reached it is whole: its call
    /// returns its result and keeps the file. A stop that came while the call waited to poll the
    /// finished job again used to delete the whole, successful export.
    #[test]
    fn an_export_that_finished_before_the_cancel_is_whole() {
        let ok = finished_job(1, Ok(())).to_json();
        assert!(ended_whole(Some(&ok)), "{ok}");
        let out = finished_result(json!({"job": 1, "path": "out.mp4"}), Some(ok.clone())).unwrap();
        assert_eq!((&out["job"], &out["result"]["frames"]), (&json!(1), &json!(24)), "{out}");
        // as bridge mode sees it, from `jobs.list`
        assert!(ended_whole(Some(&json!({"id": 1, "finished": true, "result": {"path": "out.mp4", "frames": 24}}))));
        let cancelled = finished_job(2, Err("cancelled")).to_json();
        let running = filmcraft_engine::Job { id: 3, label: "export".into(), progress: Default::default(), result: Default::default() }.to_json();
        for (what, state) in [("cancelled", Some(&cancelled)), ("still running", Some(&running)), ("unknown", None)] {
            assert!(!ended_whole(state), "{what}: {state:?}");
        }
        assert_eq!(finished_result(json!({"job": 2}), Some(cancelled)), Err(LongError::Failed("export failed: cancelled".into())));
    }

    /// A stop whose deadline comes while a call still runs deletes that call's output, unless its
    /// export finished meanwhile: then the file is whole, and kept.
    #[tokio::test]
    async fn drain_keeps_the_output_of_an_export_that_finished() {
        let dir = crate::TestDir::new("drain-finished");
        let calls = Arc::new(LongCalls::default());
        let (whole, cut) = (dir.join("whole.mp4"), dir.join("cut.mp4"));
        let started = SystemTime::now();
        for p in [&whole, &cut] {
            std::fs::write(p, b"data").unwrap();
        }
        let (done, failed) = (finished_job(1, Ok(())), finished_job(2, Err("cancelled")));
        let (a, b) = (calls.enter(None, true), calls.enter(None, true));
        a.writes(&whole.to_string_lossy(), started, Some(&done));
        b.writes(&cut.to_string_lossy(), started, Some(&failed));
        calls.stop();
        calls.drain(tokio::time::Instant::now()).await;
        assert!(whole.exists(), "a finished export is kept");
        assert!(!cut.exists(), "a cut one is deleted");
        // a call that saw its export finish (bridge mode: no handle) keeps it too
        let c = calls.enter(None, true);
        c.writes(&cut.to_string_lossy(), started, None);
        std::fs::write(&cut, b"data").unwrap();
        c.kept();
        calls.drain(tokio::time::Instant::now()).await;
        assert!(cut.exists());
        drop((a, b, c));
    }

    /// Closing an HTTP session (its client's `DELETE`) cancels that session's long calls, and no
    /// others.
    #[test]
    fn closing_a_session_cancels_only_its_calls() {
        let calls = Arc::new(LongCalls::default());
        let (a, b, stdio) = (calls.enter(Some("a".into()), true), calls.enter(Some("b".into()), true), calls.enter(None, true));
        calls.cancel_session("a");
        assert!(a.cancel.is_cancelled());
        assert!(!b.cancel.is_cancelled() && !stdio.cancel.is_cancelled());
        calls.cancel_session("unknown");
        assert!(!b.cancel.is_cancelled());
        assert_eq!(calls.count(), 3, "cancelled calls count until they end");
    }

    #[test]
    fn files_older_than_the_export_are_kept() {
        let dir = crate::TestDir::new("remove-partial-old");
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        remove_partial(&dir.join("clip.mp4").to_string_lossy(), SystemTime::now() + Duration::from_secs(60));
        assert!(dir.join("clip.mp4").exists());
    }
}
