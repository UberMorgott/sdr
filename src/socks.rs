//! ISteamNetworkingSockets P2P connections (same identity pair as the Messages session, virtual ports 1..N):
//! FFI types + `--sbench` throughput bench (host blasts on every accepted connection, client measures).
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::{log_line, Api, Identity, Msg, RtStatus, P};

/// SteamNetworkingConfigValue_t (pack 8): eValue, eDataType, union @8; 16 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CfgVal {
    pub value: i32,
    pub typ: i32,
    pub v: i64,
}

const CFG_INT32: i32 = 1; // k_ESteamNetworkingConfig_Int32

impl CfgVal {
    pub fn i32(value: i32, v: i32) -> CfgVal {
        CfgVal { value, typ: CFG_INT32, v: v as i64 }
    }
}

const _: () = {
    assert!(std::mem::size_of::<CfgVal>() == 16);
    assert!(std::mem::offset_of!(CfgVal, v) == 8);
};

pub const STATE_CONNECTING: i32 = 1;
pub const STATE_CONNECTED: i32 = 3;
pub const SEND_UNRELIABLE: i32 = 0;
pub const SEND_RELIABLE: i32 = 8;

/// Per-connection pacing: SendRateMin = SendRateMax = rate, SendBufferSize = rate x 150 ms (as the global config).
pub fn rate_cfg(rate: i32) -> [CfgVal; 3] {
    [
        CfgVal::i32(crate::CFG_SEND_RATE_MIN, rate),
        CfgVal::i32(crate::CFG_SEND_RATE_MAX, rate),
        CfgVal::i32(crate::CFG_SEND_BUFFER_SIZE, (rate as i64 * 150 / 1000).max(64 << 10) as i32),
    ]
}

pub fn set_rate(api: &Api, utils: P, h: u32, rate: i32) {
    for c in rate_cfg(rate) {
        // SAFETY: utils live; h is a connection handle (an invalid one just returns false).
        unsafe { (api.set_conn_cfg_i32)(utils, h, c.value, c.v as i32) };
    }
}

pub fn send(api: &Api, sockets: P, h: u32, d: &[u8], flags: i32) -> i32 {
    // SAFETY: sockets live, d valid for the call.
    unsafe { (api.send_conn)(sockets, h, d.as_ptr().cast(), d.len() as u32, flags, std::ptr::null_mut()) }
}

/// Drain up to 64 messages from one connection.
pub fn recv(api: &Api, sockets: P, h: u32, out: &mut Vec<Vec<u8>>) -> usize {
    let mut ptrs = [std::ptr::null_mut::<Msg>(); 64];
    // SAFETY: sockets live, ptrs has room for 64; each message released right after the copy.
    let n = unsafe { (api.recv_conn)(sockets, h, ptrs.as_mut_ptr(), 64) }.max(0) as usize;
    for &m in &ptrs[..n] {
        unsafe {
            let msg = &*m;
            out.push(std::slice::from_raw_parts(msg.data as *const u8, msg.size.max(0) as usize).to_vec());
            (api.release)(m);
        }
    }
    n
}

pub fn ping(api: &Api, sockets: P, h: u32) -> i32 {
    let mut rt: RtStatus = crate::zeroed();
    // SAFETY: sockets live, rt writable, no lanes.
    let r = unsafe { (api.conn_rt)(sockets, h, &mut rt, 0, std::ptr::null_mut()) };
    if r == 1 {
        rt.ping
    } else {
        log!("GetConnectionRealTimeStatus({h}) = {r}");
        -1
    }
}

// ------------------------------------------------------------------ accel push connections

/// Map-block push connections: host listens on virtual ports 1..=PUSH_CONNS, client connects them after a v2 ack.
pub const PUSH_CONNS: i32 = 3;
/// Total push rate to start at (bench 2026-11-11: best N x rate with loss <= 3 % in every round).
pub const PUSH_BEST: u32 = 2_400_000; // 3 x 800 KB/s: 2.27-2.31 MB/s, loss 0.2-2.1 % in 3 rounds
pub const PUSH_MIN: u32 = 500_000;
pub const PUSH_MAX: u32 = PUSH_BEST / 5 * 6;
/// Client: connections not up after this are closed; the host keeps pushing over Messages (v0.5.0 behaviour).
pub const PUSH_CONNECT_WAIT: Duration = Duration::from_secs(3);
pub const REPORT_EVERY: Duration = Duration::from_millis(250);

/// Block n goes on connection n % k.
pub fn stripe(n: u32, k: usize) -> usize {
    n as usize % k.max(1)
}

/// Host: loss-based total push rate from the client's cumulative received-block reports.
pub struct PushRate {
    pub rate: u32,
    base: Option<(u64, u64)>,
}

impl PushRate {
    pub fn new() -> PushRate {
        PushRate { rate: PUSH_BEST, base: None }
    }

    /// Client's cumulative counts over all push connections: `sent` = messages the host sent (highest seq + 1),
    /// `recv` = messages received. Some(loss) when the rate was re-evaluated.
    pub fn on_report(&mut self, sent: u64, recv: u64) -> Option<f64> {
        let Some((s0, r0)) = self.base else {
            self.base = Some((sent, recv)); // first report: baseline only
            return None;
        };
        let ds = sent.saturating_sub(s0);
        if ds < 50 {
            return None; // too few to judge; keep accumulating
        }
        self.base = Some((sent, recv));
        let loss = (1.0 - recv.saturating_sub(r0) as f64 / ds as f64).max(0.0);
        let r = if loss > 0.03 {
            self.rate as f64 * 0.8
        } else if loss < 0.01 {
            self.rate as f64 * 1.05
        } else {
            self.rate as f64
        };
        self.rate = (r as u32).clamp(PUSH_MIN, PUSH_MAX);
        Some(loss)
    }
}

// ------------------------------------------------------------------ bench

pub struct Blast {
    rate: u64,
    size: usize,
    start: Instant,
    until: Instant,
    sent: u64,
    seq: u64,
    limit: u64,
}

pub struct CConn {
    h: u32,
    up: bool,
    got: u64,
    bytes: u64,
    lo: Option<u64>,
    hi: u64,
    pings: Vec<i32>,
}

pub enum Bench {
    Host { blasts: HashMap<u32, Option<Blast>> },
    Client { n: usize, rate: u32, warm: Duration, secs: Duration, size: u16, conns: Vec<CConn>, asked: Option<Instant>, t0: Option<Instant>, next_ping: Instant },
}

fn pct(v: &mut [i32], p: usize) -> i32 {
    if v.is_empty() {
        return -1;
    }
    v.sort();
    v[(v.len() - 1) * p / 100]
}

impl Bench {
    pub fn host(api: &Api, sockets: P, n: usize) -> Bench {
        for port in 1..=n as i32 {
            // SAFETY: sockets live, no options.
            let l = unsafe { (api.listen_p2p)(sockets, port, 0, std::ptr::null()) };
            log!("SBENCH CreateListenSocketP2P(port {port}) = {l}");
        }
        Bench::Host { blasts: HashMap::new() }
    }

    pub fn client(n: usize, rate: u32, warm: u64, secs: u64, size: u16) -> Bench {
        Bench::Client { n, rate, warm: Duration::from_secs(warm), secs: Duration::from_secs(secs), size, conns: vec![], asked: None, t0: None, next_ping: Instant::now() }
    }

    /// Callback 1221.
    pub fn on_status(&mut self, api: &Api, sockets: P, h: u32, state: i32, listen: u32) {
        match self {
            Bench::Host { blasts } => {
                if state == STATE_CONNECTING && listen != 0 {
                    // SAFETY: sockets live.
                    let r = unsafe { (api.accept_conn)(sockets, h) };
                    log!("SBENCH AcceptConnection({h}) = {r}");
                    blasts.insert(h, None);
                } else if state > STATE_CONNECTED || state == 0 {
                    blasts.remove(&h);
                    // SAFETY: sockets live.
                    unsafe { (api.close_conn)(sockets, h, 0, std::ptr::null(), 0) };
                }
            }
            Bench::Client { conns, .. } => {
                if let Some(c) = conns.iter_mut().find(|c| c.h == h) {
                    c.up = state == STATE_CONNECTED;
                    if state > STATE_CONNECTED {
                        log!("SBENCH conn {h} failed state {state}");
                        crate::STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        }
    }

    /// Main loop pass. `ready`: Steam logged on + relay up; `host`: the remote host (client).
    pub fn pump(&mut self, api: &Api, sockets: P, utils: P, ready: bool, host: u64) -> bool {
        let mut busy = false;
        let mut msgs = Vec::new();
        match self {
            Bench::Host { blasts } => {
                for (&h, b) in blasts.iter_mut() {
                    msgs.clear();
                    recv(api, sockets, h, &mut msgs);
                    for m in &msgs {
                        if m.len() == 11 && m[0] == b'S' {
                            let rate = u32::from_le_bytes(m[1..5].try_into().unwrap()) as u64;
                            let secs = u32::from_le_bytes(m[5..9].try_into().unwrap()) as u64;
                            let size = u16::from_le_bytes(m[9..11].try_into().unwrap()).max(16) as usize;
                            set_rate(api, utils, h, rate as i32);
                            log!("SBENCH conn {h}: blast {rate} B/s x {secs} s, {size} B msgs");
                            let now = Instant::now();
                            *b = Some(Blast { rate, size, start: now, until: now + Duration::from_secs(secs), sent: 0, seq: 0, limit: 0 });
                        }
                    }
                    let Some(x) = b else { continue };
                    if Instant::now() > x.until {
                        log!("SBENCH conn {h}: done, sent {} msgs, LimitExceeded {}", x.seq, x.limit);
                        *b = None;
                        continue;
                    }
                    let due = (x.start.elapsed().as_secs_f64() * x.rate as f64) as u64;
                    // never burst more than 50 ms worth after a stall
                    x.sent = x.sent.max(due.saturating_sub(x.rate / 20));
                    let mut buf = vec![0u8; x.size];
                    while x.sent + x.size as u64 <= due {
                        buf[..8].copy_from_slice(&x.seq.to_le_bytes());
                        let r = send(api, sockets, h, &buf, SEND_UNRELIABLE);
                        if r != 1 {
                            x.limit += 1;
                            break;
                        }
                        x.seq += 1;
                        x.sent += x.size as u64;
                        busy = true;
                    }
                }
            }
            Bench::Client { n, rate, warm, secs, size, conns, asked, t0, next_ping } => {
                if !ready {
                    return false;
                }
                if asked.is_none() {
                    let id = Identity::steam(host);
                    let opts = rate_cfg(*rate as i32);
                    for port in 1..=*n as i32 {
                        // SAFETY: sockets live, id/opts valid for the call.
                        let h = unsafe { (api.connect_p2p)(sockets, &id, port, opts.len() as i32, opts.as_ptr()) };
                        log!("SBENCH ConnectP2P(port {port}) = {h}");
                        conns.push(CConn { h, up: false, got: 0, bytes: 0, lo: None, hi: 0, pings: vec![] });
                    }
                    *asked = Some(Instant::now());
                }
                if t0.is_none() {
                    if conns.iter().all(|c| c.up) {
                        let mut m = vec![b'S'];
                        m.extend_from_slice(&rate.to_le_bytes());
                        m.extend_from_slice(&((*warm + *secs).as_secs() as u32 + 2).to_le_bytes());
                        m.extend_from_slice(&size.to_le_bytes());
                        for c in conns.iter() {
                            send(api, sockets, c.h, &m, SEND_RELIABLE);
                        }
                        log!("SBENCH all {} connected after {:.1} s; start", conns.len(), asked.unwrap().elapsed().as_secs_f32());
                        *t0 = Some(Instant::now());
                    } else if asked.unwrap().elapsed() > Duration::from_secs(20) {
                        log!("SBENCH RESULT FAIL: not all connections up in 20 s");
                        crate::STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    return false;
                }
                let el = t0.unwrap().elapsed();
                let window = el >= *warm;
                for c in conns.iter_mut() {
                    msgs.clear();
                    busy |= recv(api, sockets, c.h, &mut msgs) > 0;
                    if !window || el > *warm + *secs {
                        continue;
                    }
                    for m in &msgs {
                        if m.len() < 8 {
                            continue;
                        }
                        let s = u64::from_le_bytes(m[..8].try_into().unwrap());
                        c.got += 1;
                        c.bytes += m.len() as u64;
                        c.lo = Some(c.lo.map_or(s, |l| l.min(s)));
                        c.hi = c.hi.max(s);
                    }
                }
                if window && Instant::now() >= *next_ping {
                    *next_ping = Instant::now() + Duration::from_millis(250);
                    for c in conns.iter_mut() {
                        let p = ping(api, sockets, c.h);
                        if p > 0 {
                            c.pings.push(p);
                        }
                    }
                }
                if el > *warm + *secs {
                    let s = secs.as_secs_f64();
                    let (mut got, mut exp, mut bytes, mut all) = (0u64, 0u64, 0u64, vec![]);
                    for (i, c) in conns.iter_mut().enumerate() {
                        let e = c.lo.map_or(0, |l| c.hi - l + 1);
                        got += c.got;
                        exp += e;
                        bytes += c.bytes;
                        all.extend_from_slice(&c.pings);
                        log!(
                            "SBENCH conn {i}: got {}/{e} loss {:.1}% goodput {:.0} B/s ping p50 {} p95 {}",
                            c.got,
                            100.0 * (1.0 - c.got as f64 / e.max(1) as f64),
                            c.bytes as f64 / s,
                            pct(&mut c.pings, 50),
                            pct(&mut c.pings, 95)
                        );
                    }
                    log!(
                        "SBENCH RESULT n={n} rate={rate} goodput={:.0} B/s loss={:.1}% rtt p50={} p95={}",
                        bytes as f64 / s,
                        100.0 * (1.0 - got as f64 / exp.max(1) as f64),
                        pct(&mut all, 50),
                        pct(&mut all, 95)
                    );
                    crate::STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn striping() {
        let k = 3;
        let mut per = [0; 3];
        for n in 0..300 {
            per[stripe(n, k)] += 1;
        }
        assert_eq!(per, [100, 100, 100]);
        assert_eq!(stripe(7, 0), 0); // no connections: never panics
    }

    #[test]
    fn rate_control() {
        let mut r = PushRate::new();
        assert_eq!(r.on_report(100, 0), None); // baseline only
        assert_eq!(r.on_report(120, 10), None); // < 50 new: wait
        assert_eq!(r.on_report(200, 100), Some(0.0)); // 100 sent, 100 got
        let up = (PUSH_BEST as f64 * 1.05) as u32;
        assert_eq!(r.rate, up);
        r.on_report(300, 190); // 10 % loss -> -20 %
        assert_eq!(r.rate, (up as f64 * 0.8) as u32);
        r.on_report(400, 288); // 2 %: hold
        assert_eq!(r.rate, (up as f64 * 0.8) as u32);
        for i in 0..20 {
            r.on_report(500 + i * 100, 388 + i * 50); // 50 % loss
        }
        assert_eq!(r.rate, PUSH_MIN);
        for i in 0..40 {
            r.on_report(3000 + i * 100, 3000 + i * 100);
        }
        assert_eq!(r.rate, PUSH_MAX);
    }
}
