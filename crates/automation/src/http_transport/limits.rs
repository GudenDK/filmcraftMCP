//! Per-peer bookkeeping: open connections (so one address cannot take every connection), failed
//! authentications (so a token cannot be guessed quickly), and log lines about refused requests (at
//! most one per peer and kind every few seconds, so a flood of bad requests cannot flood the log).

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write as _;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Where the HTTP server reports refused requests and failures: one line each, never a credential.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Peers tracked at once for rate limiting; past this, lines about new peers are dropped.
const MAX_PEERS: usize = 1024;
/// Peers whose failed authentications are tracked at once; past this, new ones are not counted.
const MAX_AUTH_PEERS: usize = 4096;

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

/// Open connections, in all and per peer. A connection is counted from accept to close, before it
/// has sent anything, so the per-peer share is what keeps one address (with or without the token)
/// from holding every connection.
pub(super) struct Conns {
    total: Arc<Semaphore>,
    max: u32,
    per_peer: usize,
    peers: Mutex<HashMap<IpAddr, usize>>,
}

/// One open connection; dropping it closes its slot.
pub(super) struct ConnSlot {
    conns: Arc<Conns>,
    peer: IpAddr,
    _total: OwnedSemaphorePermit,
}

impl Conns {
    pub(super) fn new(max: u32, per_peer: usize) -> Arc<Self> {
        Arc::new(Self { total: Arc::new(Semaphore::new(max as usize)), max, per_peer, peers: Mutex::new(HashMap::new()) })
    }

    /// A slot for a connection from `ip`, or why there is none.
    pub(super) fn open(self: &Arc<Self>, ip: IpAddr) -> Result<ConnSlot, &'static str> {
        let peer = peer_key(ip);
        let mut peers = lock(&self.peers);
        let n = peers.get(&peer).copied().unwrap_or(0);
        if n >= self.per_peer {
            return Err("too many connections from this address");
        }
        let Ok(total) = self.total.clone().try_acquire_owned() else { return Err("too many connections") };
        peers.insert(peer, n.saturating_add(1));
        Ok(ConnSlot { conns: self.clone(), peer, _total: total })
    }

    /// Wait until every connection has closed.
    pub(super) async fn all_closed(&self) {
        let _ = self.total.acquire_many(self.max).await;
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut peers = lock(&self.conns.peers);
        if let Some(n) = peers.get_mut(&self.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}

/// Failed authentications per peer: `limit` of them within `window` get the peer refused for
/// `block`, whatever token it sends then, so a token cannot be guessed at speed.
pub(super) struct AuthFailures {
    limit: u32,
    window: Duration,
    block: Duration,
    peers: Mutex<HashMap<IpAddr, Failures>>,
}

struct Failures {
    since: Instant,
    count: u32,
    blocked_until: Option<Instant>,
}

impl AuthFailures {
    pub(super) fn new(limit: u32, window: Duration, block: Duration) -> Self {
        Self { limit: limit.max(1), window, block, peers: Mutex::new(HashMap::new()) }
    }

    /// How much longer `ip` is refused, if it is.
    pub(super) fn blocked(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let until = lock(&self.peers).get(&peer_key(ip))?.blocked_until?;
        (until > now).then(|| until.duration_since(now))
    }

    /// Count a failed authentication from `ip`. True when it starts a block.
    pub(super) fn failed(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let key = peer_key(ip);
        let mut peers = lock(&self.peers);
        if !peers.contains_key(&key) && peers.len() >= MAX_AUTH_PEERS {
            peers.retain(|_, f| f.blocked_until.is_some_and(|u| u > now) || now.duration_since(f.since) < self.window);
            if peers.len() >= MAX_AUTH_PEERS {
                return false;
            }
        }
        let f = peers.entry(key).or_insert(Failures { since: now, count: 0, blocked_until: None });
        if now.duration_since(f.since) >= self.window || f.blocked_until.is_some_and(|u| u <= now) {
            *f = Failures { since: now, count: 0, blocked_until: None };
        }
        f.count = f.count.saturating_add(1);
        if f.count < self.limit || f.blocked_until.is_some() {
            return false;
        }
        f.blocked_until = now.checked_add(self.block).or_else(|| now.checked_add(Duration::from_secs(86_400)));
        true
    }
}

/// Tell a client there is no room for its connection, instead of closing it unanswered: a 503 it
/// can retry after a second. Bounded in time; what it sent is read and dropped, so closing the
/// socket does not reset it before the reply is read.
pub(super) async fn refuse_connection(mut stream: tokio::net::TcpStream) {
    const REPLY: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nConnection: close\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 41\r\n\r\nService Unavailable: too many connections";
    let reply = async {
        stream.write_all(REPLY).await?;
        stream.shutdown().await?;
        let mut sink = [0u8; 4096];
        let mut left: usize = 64 << 10;
        while left > 0 {
            let n = stream.read(&mut sink).await?;
            if n == 0 {
                break;
            }
            left = left.saturating_sub(n);
        }
        Ok::<_, std::io::Error>(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), reply).await;
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
    fn connections_are_counted_per_peer_and_in_all() {
        let conns = Conns::new(4, 2);
        let (a, b, c): (IpAddr, IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let a1 = conns.open(a).unwrap();
        let _a2 = conns.open(a).unwrap();
        assert_eq!(conns.open(a).err(), Some("too many connections from this address"));
        let _b1 = conns.open(b).unwrap();
        let _b2 = conns.open(b).unwrap();
        assert_eq!(conns.open(c).err(), Some("too many connections"));
        drop(a1);
        assert!(conns.open(c).is_ok(), "a closed connection frees its slot");
        assert_eq!(lock(&conns.peers).len(), 2, "c's slot was dropped at once");
    }

    #[test]
    fn failed_authentications_block_a_peer_for_a_while() {
        let auth = AuthFailures::new(3, Duration::from_secs(60), Duration::from_millis(300));
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        assert!(!auth.failed(a) && !auth.failed(a));
        assert_eq!(auth.blocked(a), None);
        assert!(auth.failed(a), "the third failure starts a block");
        assert!(auth.blocked(a).is_some_and(|d| d <= Duration::from_millis(300)));
        assert_eq!(auth.blocked(b), None, "other peers are not affected");
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(auth.blocked(a), None, "the block ends");
        assert!(!auth.failed(a), "and the count starts over");
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
