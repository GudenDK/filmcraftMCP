//! Per-peer bookkeeping: open connections (so one address cannot take every connection, and
//! connections that send nothing valid cannot lock out the ones that do), failed authentications
//! (so a short token cannot be guessed quickly), and log lines about refused requests (at most one
//! per peer and kind every few seconds, so a flood of bad requests cannot flood the log).

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Where the HTTP server reports refused requests and failures: one line each, never a credential.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Peers tracked at once for rate limiting; past this, lines about new peers are dropped.
const MAX_PEERS: usize = 1024;
/// Peers whose failed authentications are tracked at once; past this, new ones are not counted.
const MAX_AUTH_PEERS: usize = 4096;

/// The address a limit applies to: an IPv4 address, or an IPv6 /64 (one host's or one LAN's
/// share; a client can pick any address in it). An IPv4-mapped IPv6 address counts as IPv4. Every
/// loopback address (127.0.0.0/8, ::1) is one peer, this machine: a local process can send from
/// any of them.
pub(super) fn peer_key(ip: IpAddr) -> IpAddr {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    };
    if ip.is_loopback() {
        return IpAddr::V4(Ipv4Addr::LOCALHOST);
    }
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open connections, in all and per peer. A connection is counted from accept to close, before it
/// has sent anything. At a limit, the oldest connection (of that peer, or of all) that has not
/// yet made a request the server accepted (past the token, `Host` and `Origin` checks) is closed
/// to make room, so that connections that send nothing, or nothing valid, cannot lock out clients
/// that have the token, from another address or from the same one (every local client is
/// 127.0.0.1). Only when every connection there has proven itself is the new one refused. This
/// machine (the loopback peer) has no share of its own: every local client, and every client a
/// tunnel or proxy on it relays, comes from there, so it may use every connection.
pub(super) struct Conns {
    max: usize,
    per_peer: usize,
    inner: Mutex<Open>,
    /// How many connections are open (for [`Self::all_closed`]).
    count: tokio::sync::watch::Sender<usize>,
}

#[derive(Default)]
struct Open {
    next: u64,
    /// By age: ids only grow.
    conns: std::collections::BTreeMap<u64, Conn>,
    peers: HashMap<IpAddr, usize>,
}

struct Conn {
    peer: IpAddr,
    trusted: Arc<AtomicBool>,
    close: CancellationToken,
}

/// One open connection; dropping it closes its slot.
pub(super) struct ConnSlot {
    conns: Arc<Conns>,
    id: u64,
    trusted: Arc<AtomicBool>,
    close: CancellationToken,
}

impl ConnSlot {
    /// Set once a request on this connection passed the token, `Host` and `Origin` checks: it is
    /// no longer closed to make room.
    pub(super) fn trusted(&self) -> Arc<AtomicBool> {
        self.trusted.clone()
    }

    /// Resolves when this connection was closed to make room for another.
    pub(super) async fn made_room(&self) {
        self.close.cancelled().await;
    }
}

impl Conns {
    pub(super) fn new(max: u32, per_peer: usize) -> Arc<Self> {
        Arc::new(Self { max: (max as usize).max(1), per_peer: per_peer.max(1), inner: Mutex::new(Open::default()), count: tokio::sync::watch::Sender::new(0) })
    }

    /// A slot for a connection from `ip` (another connection may be closed to make room), or why
    /// there is none.
    pub(super) fn open(self: &Arc<Self>, ip: IpAddr) -> Result<ConnSlot, &'static str> {
        let peer = peer_key(ip);
        let per_peer = if peer.is_loopback() { self.max } else { self.per_peer };
        let mut open = lock(&self.inner);
        if open.peers.get(&peer).copied().unwrap_or(0) >= per_peer && !open.make_room(|c| c.peer == peer) {
            return Err("too many connections from this address");
        }
        if open.conns.len() >= self.max && !open.make_room(|_| true) {
            return Err("too many connections");
        }
        let id = open.next;
        open.next = open.next.wrapping_add(1);
        let (trusted, close) = (Arc::new(AtomicBool::new(false)), CancellationToken::new());
        open.conns.insert(id, Conn { peer, trusted: trusted.clone(), close: close.clone() });
        let n = open.peers.entry(peer).or_insert(0);
        *n = n.saturating_add(1);
        self.count.send_replace(open.conns.len());
        Ok(ConnSlot { conns: self.clone(), id, trusted, close })
    }

    /// Wait until every connection has closed.
    pub(super) async fn all_closed(&self) {
        let _ = self.count.subscribe().wait_for(|n| *n == 0).await;
    }
}

impl Open {
    /// Close the oldest connection that `among` picks and that has not proven itself; false when
    /// there is none.
    fn make_room(&mut self, among: impl Fn(&Conn) -> bool) -> bool {
        let Some(id) = self.conns.iter().find(|(_, c)| among(c) && !c.trusted.load(Ordering::Relaxed)).map(|(id, _)| *id) else { return false };
        self.remove(id);
        true
    }

    fn remove(&mut self, id: u64) {
        let Some(c) = self.conns.remove(&id) else { return };
        c.close.cancel();
        if let Some(n) = self.peers.get_mut(&c.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.peers.remove(&c.peer);
            }
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut open = lock(&self.conns.inner);
        open.remove(self.id);
        self.conns.count.send_replace(open.conns.len());
    }
}

/// Failed authentications per peer: `limit` of them within `window` get the peer blocked for
/// `block` (the gate decides what that refuses), so a token cannot be guessed at speed.
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
        assert_eq!(k("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), "2001:db8:1:2::");
        assert_eq!(k("2001:db8:1:2::9"), k("2001:db8:1:2:ffff::1"));
        // this machine is one peer, whichever loopback address a process sends from
        for local in ["127.0.0.1", "127.0.0.2", "127.255.0.9", "::1", "::ffff:127.0.0.3"] {
            assert_eq!(k(local), "127.0.0.1", "{local}");
        }
    }

    #[test]
    fn connections_are_counted_per_peer_and_in_all() {
        let conns = Conns::new(4, 2);
        let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
        let proven = |slot: ConnSlot| {
            slot.trusted().store(true, Ordering::Relaxed);
            slot
        };
        let _a1 = proven(conns.open(ip("10.0.0.1")).unwrap());
        let _a2 = proven(conns.open(ip("10.0.0.1")).unwrap());
        assert_eq!(conns.open(ip("10.0.0.1")).err(), Some("too many connections from this address"), "both proved themselves");
        let b1 = conns.open(ip("10.0.0.2")).unwrap();
        let b2 = conns.open(ip("10.0.0.2")).unwrap();
        // full: the oldest connection that has not proven itself makes room
        let c1 = conns.open(ip("10.0.0.3")).unwrap();
        assert!(b1.close.is_cancelled() && !b2.close.is_cancelled() && !c1.close.is_cancelled());
        drop(b1); // its task ends: nothing more to count
        assert_eq!(lock(&conns.inner).conns.len(), 4);
        assert_eq!(lock(&conns.inner).peers.get(&ip("10.0.0.2")), Some(&1));
        let _b2 = proven(b2);
        let _c1 = proven(c1);
        assert_eq!(conns.open(ip("10.0.0.4")).err(), Some("too many connections"), "every one proved itself");
    }

    /// Connections that send nothing from several loopback addresses cannot lock local clients
    /// out: they are one peer, and the oldest of them make room. This machine has no share of its
    /// own (local clients, and those a tunnel relays, all come from it): it may use every
    /// connection, but never take those that proved themselves.
    #[test]
    fn silent_local_connections_make_room() {
        let conns = Conns::new(64, 8);
        let proven = conns.open("10.0.0.1".parse().unwrap()).unwrap();
        proven.trusted().store(true, Ordering::Relaxed);
        let mut silent = Vec::new();
        for i in 0..8u8 {
            for _ in 0..12 {
                if let Ok(s) = conns.open(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2 + i))) {
                    silent.push(s);
                }
            }
        }
        let open = |slots: &[ConnSlot]| slots.iter().filter(|s| !s.close.is_cancelled()).count();
        assert_eq!((silent.len(), open(&silent), lock(&conns.inner).conns.len()), (96, 63, 64), "the oldest closed, the proven one kept");
        assert!(!proven.close.is_cancelled());
        let client = conns.open(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(open(&silent), 62, "a silent one made room");
        assert!(!client.close.is_cancelled());
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
