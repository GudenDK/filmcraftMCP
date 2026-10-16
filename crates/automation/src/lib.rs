//! # filmcraft-automation
//!
//! The FilmCraft MCP server (official `rmcp` SDK): stdio ([`FilmcraftMcp::serve_stdio`]), or
//! Streamable HTTP at `http://<addr>/mcp` ([`FilmcraftMcp::serve_http`], [`http_transport`]) where
//! any number of clients share one backend. Both stop on a future the caller gives them
//! ([`Stop`]), cancelling calls in flight and deleting partial exports.
//!
//! * **Headless** mode drives an in-process [`filmcraft_engine::Session`]: every engine command,
//!   project/sequence inspection and frame rendering (PNG) — no window needed.
//! * **Bridge** mode forwards to a running desktop app over the JSON-lines control channel
//!   (`filmcraft --control <port>`), so agents can also inspect, screenshot, click, drag, scroll
//!   and type in the live UI — every menu, panel, timeline gesture and keystroke.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod bridge;
pub mod http_transport;
pub mod long_job;
pub mod server;
mod shots;

/// A test folder under the system's temporary folder, removed when the test ends, also when it
/// fails or times out: a failing export test used to leave its partial MP4s behind.
#[cfg(test)]
pub(crate) struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("filmcraft-mcp-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub use bridge::BridgeClient;
pub use http_transport::HttpOptions;
pub use server::FilmcraftMcp;

/// How a server stops once the future it was given to stop on resolves (`()` means
/// [`Stop::Graceful`]): calls in flight are cancelled, and it waits a while for cancelled exports to
/// delete their partial files; what one still running at the end wrote is deleted then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Asked to stop (Ctrl+C, SIGTERM): wait up to 15 s.
    Graceful,
    /// The process is about to be killed (on Windows: its console closing, the user logging off,
    /// the system shutting down, each about 5 s before the kill): wait at most 3 s.
    Hurried,
}

impl Stop {
    /// How long to wait for cancelled calls.
    pub fn grace(self) -> std::time::Duration {
        std::time::Duration::from_secs(match self {
            Stop::Graceful => 15,
            Stop::Hurried => 3,
        })
    }
}

impl From<()> for Stop {
    fn from((): ()) -> Self {
        Stop::Graceful
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Engine(#[from] filmcraft_engine::EngineError),
    #[error("bridge: {0}")]
    Bridge(String),
    #[error("app: {0}")]
    App(String),
    #[error("{0}")]
    Other(String),
}

/// Encode RGBA8 as PNG, downscaled so the longest side is at most `max_side` (0 = no limit).
pub fn png_rgba(w: u32, h: u32, rgba: Vec<u8>, max_side: u32) -> Result<Vec<u8>, AutomationError> {
    let mut img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| AutomationError::Other("bad image".into()))?;
    if max_side > 0 && w.max(h) > max_side {
        let s = max_side as f32 / w.max(h) as f32;
        img = image::imageops::resize(&img, ((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1), image::imageops::FilterType::Triangle);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| AutomationError::Other(e.to_string()))?;
    Ok(out.into_inner())
}
