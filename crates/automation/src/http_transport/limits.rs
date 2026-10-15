//! Per-peer bookkeeping: log lines about refused requests, at most one per peer and kind every few
//! seconds, so a flood of bad requests cannot flood the log.

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write as _;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Where the HTTP server reports refused requests and failures: one line each, never a credential.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Peers tracked at once for rate limiting; past this, lines about new peers are dropped.
const MAX_PEERS: usize = 1024;

/// The address a limit applies to: an IPv4 address, or an IPv6 /64 (one host's or one LAN's
/// share; a client can pick any address in it). An IPv4-mapped IPv6 address counts as IPv4.
pub(super) fn peer_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None if v6.is_loopback() => ip,
            None => {
                let [a, b, c, d, ..] = v6.segments();
                IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
            }
        },
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server's log: stderr, or the [`LogSink`] in [`super::HttpOptions::log`].
pub(super) struct Log {
    sink: Option<LogSink>,
    every: Duration,
    recent: Mutex<HashMap<(IpAddr, &'static str), Recent>>,
}

struct Recent {
    at: Instant,
    left_out: u32,
}

impl Log {
    pub(super) fn new(sink: Option<LogSink>, every: Duration) -> Self {
        Self { sink, every, recent: Mutex::new(HashMap::new()) }
    }

    /// One line about `peer`, unless a line of the same `kind` about it went out less than `every`
    /// ago: then it is counted, and the next line says how many were left out.
    pub(super) fn event(&self, peer: IpAddr, kind: &'static str, msg: impl Display) {
        let now = Instant::now();
        let key = (peer_key(peer), kind);
        let left_out = {
            let mut recent = lock(&self.recent);
            match recent.get_mut(&key) {
                Some(r) if now.duration_since(r.at) < self.every => {
                    r.left_out = r.left_out.saturating_add(1);
                    return;
                }
                Some(r) => std::mem::replace(r, Recent { at: now, left_out: 0 }).left_out,
                None => {
                    if recent.len() >= MAX_PEERS {
                        recent.retain(|_, r| now.duration_since(r.at) < self.every);
                    }
                    if recent.len() >= MAX_PEERS {
                        return; // bad requests from over a thousand addresses at once: drop the line
                    }
                    recent.insert(key, Recent { at: now, left_out: 0 });
                    0
                }
            }
        };
        match left_out {
            0 => self.line(format_args!("{msg}")),
            n => self.line(format_args!("{msg} (and {n} more like it since the last such line)")),
        }
    }

    /// One line, not rate limited.
    pub(super) fn line(&self, msg: std::fmt::Arguments) {
        let line = format!("filmcraft-mcp: {msg}");
        match &self.sink {
            Some(sink) => sink(&line),
            // `eprintln!` panics when stderr is gone
            None => {
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_are_grouped_by_address_and_ipv6_prefix() {
        let k = |s: &str| peer_key(s.parse().unwrap()).to_string();
        assert_eq!(k("192.168.1.5"), "192.168.1.5");
        assert_eq!(k("::ffff:10.0.0.1"), "10.0.0.1");
        assert_eq!(k("::1"), "::1");
        assert_eq!(k("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), "2001:db8:1:2::");
        assert_eq!(k("2001:db8:1:2::9"), k("2001:db8:1:2:ffff::1"));
    }

    #[test]
    fn lines_are_rate_limited_per_peer_and_kind() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let log = Log::new(Some(Arc::new(move |l: &str| sink.lock().unwrap().push(l.to_string()))), Duration::from_millis(200));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..5 {
            log.event(a, "401", "refused a");
        }
        log.event(a, "403", "forbidden a");
        log.event(b, "401", "refused b");
        assert_eq!(lines.lock().unwrap().len(), 3, "{:?}", lines.lock().unwrap());
        std::thread::sleep(Duration::from_millis(250));
        log.event(a, "401", "refused a again");
        let l = lines.lock().unwrap();
        assert_eq!(l.last().map(String::as_str), Some("filmcraft-mcp: refused a again (and 4 more like it since the last such line)"), "{l:?}");
    }
}
