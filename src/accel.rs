//! Map-download accelerator (`--no-accel` turns it off).
//! Factorio 2.1.24 wire format (verified on a capture): byte 0 low 5 bits = message type, 0x20 = random flag;
//! TransferBlockRequest = type 12, exactly [b0][u32 LE block]; TransferBlock = type 13, [b0][u32 LE block][data],
//! every block the same size (the last one zero-padded). The game asks for blocks under a congestion window and
//! backs off when the tunnel queue makes its requests time out. Host: once the client says hello (SDR channel 1,
//! so an old host never sees it), prefetches every block from the local server and pushes it at tunnel speed.
//! Client: caches pushed blocks, answers the game locally, re-requests pushes that got lost.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::log_line;

const REQ: u8 = 12;
const BLK: u8 = 13;
const CONNECT: u8 = 2; // ConnectionRequest: a (re)join -> new map, block numbers restart
const IDLE_RESET: Duration = Duration::from_secs(30);
const CACHE_MAX: usize = 64 << 20;
const WINDOW: usize = 32;
const RETRY: Duration = Duration::from_millis(200);
const TRIES: u8 = 3;
/// Client: a block this far behind the newest push is lost, not reordered.
const LOST_GAP: u32 = 64;
const LOST_REASK: Duration = Duration::from_millis(300);
/// Client: a wanted block the host has not pushed yet is asked for anyway after this (host stopped pushing?).
const STALE_REASK: Duration = Duration::from_millis(1000);
const MISSING_MAX: usize = 1 << 16;
/// Client: no push for this long = the stream ended or stopped -> forward the game's asks again.
const PUSH_STALL: Duration = Duration::from_millis(300);
const HELLO_EVERY: Duration = Duration::from_millis(500);
pub const HELLO: &[u8] = b"sdr-accel-1";
/// v2 (sent next to HELLO; an old host ignores it): host acks with HELLO2 = it takes extra Sockets push connections.
pub const HELLO2: &[u8] = b"sdr-accel-2";
/// Client -> host on the control channel: REPORT + u64 LE received + u64 LE sent (highest seq + 1), over all push connections.
pub const REPORT: &[u8] = b"sdr-rx";

#[derive(Debug, PartialEq)]
pub enum Pkt {
    Req(u32),
    Blk(u32),
    Connect,
    Other,
    Bad,
}

pub fn parse(d: &[u8]) -> Pkt {
    let Some(&b0) = d.first() else { return Pkt::Other };
    match b0 & 0x1f {
        t @ (REQ | BLK) => {
            // Fragmented/last flags or odd sizes never seen on these types: refuse to guess.
            if b0 & 0xc0 != 0 || (t == REQ && d.len() != 5) || (t == BLK && d.len() < 6) {
                return Pkt::Bad;
            }
            let n = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
            if t == REQ { Pkt::Req(n) } else { Pkt::Blk(n) }
        }
        CONNECT => Pkt::Connect,
        _ => Pkt::Other,
    }
}

pub fn request(n: u32) -> Vec<u8> {
    let mut v = vec![REQ];
    v.extend_from_slice(&n.to_le_bytes());
    v
}

#[derive(Debug, PartialEq)]
pub enum Act {
    Pass,
    Drop,
    /// Answer back towards the requester with these bytes (host: over the tunnel; client: to the game).
    Reply(Vec<u8>),
}

pub struct Accel {
    pub on: bool,
    tag: String,
    last: Instant,
    cache: HashMap<u32, Vec<u8>>,
    bytes: usize,
    /// host: the client said hello for this download
    hello: bool,
    /// host: blocks requested from the server, (last request, tries)
    pending: HashMap<u32, (Instant, u8)>,
    /// host: next block to prefetch; None = no download running
    next: Option<u32>,
    /// host: first block the server never answered (past the end of the map)
    end: Option<u32>,
    /// client: asked for by the game, not delivered yet -> last time forwarded (or first asked)
    wanted: HashMap<u32, Instant>,
    /// client: newest pushed block seen (Some = the host pushes)
    hi: Option<u32>,
    /// client: host acked the hello
    pushing: bool,
    last_push: Instant,
    /// client: blocks skipped by the push stream -> last time re-asked
    missing: HashMap<u32, Option<Instant>>,
    hello_at: Option<Instant>,
    hits: u64,
    misses: u64,
    started: Option<Instant>,
}

impl Accel {
    pub fn new(on: bool, tag: String) -> Accel {
        Accel {
            on,
            tag,
            last: Instant::now(),
            cache: HashMap::new(),
            bytes: 0,
            hello: false,
            pending: HashMap::new(),
            next: None,
            end: None,
            wanted: HashMap::new(),
            hi: None,
            missing: HashMap::new(),
            pushing: false,
            last_push: Instant::now(),
            hello_at: None,
            hits: 0,
            misses: 0,
            started: None,
        }
    }

    fn reset(&mut self, why: &str) {
        if let Some(s) = self.started.take() {
            log!(
                "ACCEL {}: {why}: reset after {:.1} s; cached {} blocks / {} B, hits {} misses {}, pushed {}, end {:?}",
                self.tag,
                s.elapsed().as_secs_f32(),
                self.cache.len(),
                self.bytes,
                self.hits,
                self.misses,
                self.hello || self.pushing,
                self.end
            );
        }
        *self = Accel::new(self.on, std::mem::take(&mut self.tag));
    }

    fn disable(&mut self, why: &str) {
        log!("ACCEL {}: DISABLED ({why}); plain forwarding from now on", self.tag);
        self.reset(why);
        self.on = false;
    }

    fn odd(&mut self, d: &[u8]) {
        self.disable(&format!("odd request/block datagram, {} bytes, b0={:#04x}", d.len(), d[0]));
    }

    /// Every download packet: forget a download idle for IDLE_RESET (next one is a different map).
    fn touch(&mut self) {
        if self.started.is_some() && self.last.elapsed() > IDLE_RESET {
            self.reset("idle");
        }
        if self.started.is_none() {
            self.started = Some(Instant::now());
            log!("ACCEL {}: download started", self.tag);
        }
        self.last = Instant::now();
    }

    /// Insert a block; false if the cache is full (accel then disabled).
    fn store(&mut self, n: u32, d: &[u8]) -> bool {
        if self.cache.contains_key(&n) {
            return true;
        }
        if self.bytes + d.len() > CACHE_MAX {
            self.disable("cache full");
            return false;
        }
        self.bytes += d.len();
        self.cache.insert(n, d.to_vec());
        true
    }

    /// Host: hello from the client on the control channel. True: answer with HELLO (we push).
    pub fn host_hello(&mut self) -> bool {
        if self.on && !self.hello {
            self.hello = true;
            self.touch();
            log!("ACCEL {}: client hello -> prefetch + push", self.tag);
        }
        self.on
    }

    /// Client: the host acked our hello -> it pushes every block from now on.
    pub fn client_ack(&mut self) {
        if self.on && !self.pushing && self.started.is_some() {
            self.pushing = true;
            self.last_push = Instant::now();
            log!("ACCEL {}: host acked -> it pushes", self.tag);
        }
    }

    fn stalled(&self) -> bool {
        self.last_push.elapsed() > PUSH_STALL
    }

    /// Host, client -> server datagram.
    pub fn host_from_client(&mut self, d: &[u8]) -> Act {
        if !self.on {
            return Act::Pass;
        }
        match parse(d) {
            Pkt::Bad => self.odd(d),
            Pkt::Connect => self.reset("client (re)connect"),
            Pkt::Req(n) if self.hello => {
                self.touch();
                if self.next.is_none() {
                    self.next = Some(0);
                }
                if self.end.is_some_and(|e| n >= e) {
                    self.end = None; // our end guess was wrong: the game knows the size
                }
                // The client only asks for what it lacks (lost push or not pushed yet).
                if let Some(b) = self.cache.get(&n) {
                    self.hits += 1;
                    return Act::Reply(b.clone());
                }
                if self.pending.contains_key(&n) {
                    return Act::Drop;
                }
                self.misses += 1;
                self.pending.insert(n, (Instant::now(), 1));
            }
            _ => {}
        }
        Act::Pass
    }

    /// Host, server -> client datagram: forward it?
    pub fn host_from_server(&mut self, d: &[u8]) -> bool {
        if !self.on {
            return true;
        }
        match parse(d) {
            Pkt::Bad => {
                self.odd(d);
                true
            }
            Pkt::Blk(n) if self.next.is_some() => {
                self.last = Instant::now();
                self.pending.remove(&n);
                if self.cache.contains_key(&n) {
                    return false; // duplicate answer to our retry
                }
                self.store(n, d);
                true
            }
            _ => true,
        }
    }

    /// Host, every loop pass: requests to send to the server. `room`: the tunnel takes more now.
    pub fn host_prefetch(&mut self, room: bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(mut next) = self.next.filter(|_| self.on) else { return out };
        if self.last.elapsed() > IDLE_RESET {
            self.reset("idle");
            return out;
        }
        let now = Instant::now();
        let mut end = self.end;
        self.pending.retain(|&n, (t, tries)| {
            if t.elapsed() < RETRY {
                return true;
            }
            if *tries >= TRIES {
                end = Some(end.map_or(n, |e| e.min(n)));
                return false;
            }
            *tries += 1;
            *t = now;
            out.push(request(n));
            true
        });
        if end != self.end {
            log!("ACCEL {}: no answer for block {:?} -> assuming end of map; {} blocks cached", self.tag, end, self.cache.len());
            self.end = end;
        }
        if room && self.end.is_none() {
            while self.pending.len() < WINDOW {
                let n = next;
                next += 1;
                if !self.cache.contains_key(&n) && !self.pending.contains_key(&n) {
                    self.pending.insert(n, (now, 1));
                    out.push(request(n));
                }
            }
            self.next = Some(next);
        }
        out
    }

    /// Client, game -> host datagram.
    pub fn client_from_game(&mut self, d: &[u8]) -> Act {
        if !self.on {
            return Act::Pass;
        }
        match parse(d) {
            Pkt::Bad => self.odd(d),
            Pkt::Connect => self.reset("game (re)connect"),
            Pkt::Req(n) => {
                self.touch();
                if let Some(b) = self.cache.get(&n) {
                    self.hits += 1;
                    return Act::Reply(b.clone());
                }
                self.misses += 1;
                let now = Instant::now();
                if self.pushing && !self.stalled() {
                    // on its way, or lost and tick() re-asks
                    self.wanted.entry(n).or_insert(now);
                    return Act::Drop;
                }
                if self.pushing && self.wanted.get(&n).is_some_and(|t| t.elapsed() < LOST_REASK) {
                    return Act::Drop;
                }
                self.wanted.insert(n, now); // plain forwarding: old host, or the push stream ended/stopped
            }
            Pkt::Blk(_) | Pkt::Other => {}
        }
        Act::Pass
    }

    /// Client: send the hello now? (download running, host not pushing yet, at most every HELLO_EVERY)
    pub fn client_hello(&mut self) -> bool {
        if !self.on || self.started.is_none() || self.pushing || self.hello_at.is_some_and(|t| t.elapsed() < HELLO_EVERY) {
            return false;
        }
        self.hello_at = Some(Instant::now());
        true
    }

    /// Client, host -> game datagram: deliver it to the game? Pushed blocks nobody asked for only go to the cache.
    pub fn client_from_tunnel(&mut self, d: &[u8]) -> bool {
        if !self.on {
            return true;
        }
        match parse(d) {
            Pkt::Bad => {
                self.odd(d);
                true
            }
            Pkt::Blk(n) => {
                self.touch();
                let wanted = self.wanted.remove(&n).is_some();
                if !wanted && self.cache.contains_key(&n) {
                    return false;
                }
                self.missing.remove(&n);
                if self.pushing {
                    self.last_push = Instant::now();
                    match self.hi {
                        Some(h) if n > h + 1 => {
                            if n - h > MISSING_MAX as u32 || self.missing.len() > MISSING_MAX {
                                self.odd(d);
                                return true;
                            }
                            self.missing.extend((h + 1..n).filter(|m| !self.cache.contains_key(m)).map(|m| (m, None)));
                        }
                        _ => {}
                    }
                    self.hi = Some(self.hi.map_or(n, |h| h.max(n)));
                }
                // cache full -> accel off -> deliver everything again
                !self.store(n, d) || wanted
            }
            _ => true,
        }
    }

    /// Client, every loop pass: requests to forward (lost pushes, stale asks); drops an idle download.
    pub fn tick(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.started.is_some() && self.last.elapsed() > IDLE_RESET {
            self.reset("idle");
        }
        if !self.pushing {
            return out;
        }
        let (hi, stalled, now) = (self.hi.unwrap_or(0), self.stalled(), Instant::now());
        for (&n, t) in self.missing.iter_mut() {
            if n.saturating_add(LOST_GAP) <= hi && t.is_none_or(|t| t.elapsed() >= LOST_REASK) {
                *t = Some(now);
                out.push(request(n));
            }
        }
        // asked for, neither pushed nor known lost: the push stream ended / host stopped (end guess, cache full)
        for (&n, t) in self.wanted.iter_mut() {
            if t.elapsed() >= if stalled { LOST_REASK } else { STALE_REASK } && !self.missing.contains_key(&n) {
                *t = now;
                out.push(request(n));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(n: u32) -> Vec<u8> {
        let mut v = vec![0x2d];
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&[7u8; 503]);
        v
    }

    #[test]
    fn parse_strict() {
        assert_eq!(parse(&[0x2c, 3, 0, 0, 0]), Pkt::Req(3));
        assert_eq!(parse(&request(0x01020304)), Pkt::Req(0x01020304));
        assert_eq!(parse(&blk(9)), Pkt::Blk(9));
        assert_eq!(parse(&[0x0c, 1, 0, 0, 0, 0]), Pkt::Bad);
        assert_eq!(parse(&[0x4d, 1, 0, 0, 0, 9]), Pkt::Bad);
        assert_eq!(parse(&[0x22, 0]), Pkt::Connect);
        assert_eq!(parse(&[0x06, 0]), Pkt::Other);
        assert_eq!(parse(&[]), Pkt::Other);
    }

    #[test]
    fn host_flow() {
        let mut a = Accel::new(true, "t".into());
        assert_eq!(a.host_from_client(&request(0)), Act::Pass);
        assert!(a.host_prefetch(true).is_empty()); // no hello: plain forwarding only
        a.host_hello();
        assert_eq!(a.host_from_client(&request(0)), Act::Pass);
        assert_eq!(a.host_prefetch(true).len(), WINDOW - 1); // block 0 pending (client's own request)
        assert!(a.host_from_server(&blk(0)));
        assert!(!a.host_from_server(&blk(0))); // duplicate
        assert_eq!(a.host_from_client(&request(5)), Act::Drop); // pending
        assert_eq!(a.host_from_client(&request(0)), Act::Reply(blk(0))); // lost push
        assert_eq!(a.host_from_client(&[0x22, 0, 0]), Act::Pass);
        assert!(a.next.is_none() && a.cache.is_empty() && !a.hello); // reset on reconnect
    }

    #[test]
    fn host_end_of_map() {
        let mut a = Accel::new(true, "t".into());
        a.host_hello();
        a.host_from_client(&request(0));
        a.host_prefetch(true);
        for _ in 0..TRIES {
            for (_, (t, _)) in a.pending.iter_mut() {
                *t = Instant::now() - RETRY;
            }
            a.host_prefetch(true);
        }
        assert_eq!(a.end, Some(0));
        assert!(a.host_prefetch(true).is_empty());
        a.host_from_client(&request(40)); // the game knows better
        assert_eq!(a.end, None);
    }

    #[test]
    fn client_flow() {
        let mut a = Accel::new(true, "t".into());
        assert_eq!(a.client_from_game(&request(0)), Act::Pass); // nothing pushed yet: forward
        assert!(a.client_hello() && !a.client_hello());
        assert!(a.client_from_tunnel(&blk(0))); // asked for -> delivered
        a.client_ack();
        assert!(!a.client_hello()); // acked: no more hellos
        assert!(!a.client_from_tunnel(&blk(1))); // unsolicited push -> cache only
        assert_eq!(a.client_from_game(&request(1)), Act::Reply(blk(1)));
        assert_eq!(a.client_from_game(&request(2)), Act::Drop); // on its way
        assert!(a.client_from_tunnel(&blk(2))); // delivered when it lands
        assert!(!a.client_from_tunnel(&blk(200))); // 3..199 lost or late
        assert_eq!(a.client_from_game(&request(3)), Act::Drop); // never forwarded while pushing
        assert!(!a.client_from_tunnel(&blk(150))); // late, not lost
        let mut asks = a.tick();
        asks.sort();
        assert_eq!(asks.len(), 134); // 3..=136 lost (gap >= 64 behind 200); 137..199 may be in flight, 150 arrived
        assert!(a.tick().is_empty()); // re-ask only after LOST_REASK
        assert!(a.client_from_tunnel(&blk(3))); // the game asked for 3: delivered
        a.wanted.insert(300, Instant::now() - STALE_REASK);
        assert_eq!(a.tick(), vec![request(300)]); // host stopped pushing?
        a.last_push = Instant::now() - PUSH_STALL * 2;
        assert_eq!(a.client_from_game(&request(301)), Act::Pass); // stream ended: plain forwarding
        let mut off = Accel::new(false, "t".into());
        assert!(off.client_from_tunnel(&blk(1)));
        assert_eq!(off.client_from_game(&request(1)), Act::Pass);
        assert!(!off.client_hello());
    }
}
