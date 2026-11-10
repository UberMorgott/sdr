//! sdr-tunnel: UDP <-> Steam Datagram Relay tunnel with a very verbose log of
//! every Steam step.
//!
//! host:   anonymous Steam game server (no Steam client/account needed);
//!         peers arrive on the game server's ISteamNetworkingMessages, each gets
//!         its own local UDP socket forwarding to --target.
//! client: logged-in Steam client; a local UDP socket (--listen) forwarded to
//!         the host's SteamID over SDR.
//!
//! Steam is driven through the flat C API of steam_api64.dll (loaded from the
//! exe dir), callbacks through manual dispatch so every callback is logged by id.
//! Struct layouts: Steamworks SDK headers (steamnetworkingtypes.h,
//! isteamnetworkingutils.h, isteamnetworkingsockets.h, steam_api_internal.h,
//! steamtypes.h); Identity/Msg/ConnInfo as in Wartales winmm/src/steam/sdr.rs.

mod names;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

// ------------------------------------------------------------------ logging

static LOG: Mutex<Option<File>> = Mutex::new(None);
static STOP: AtomicBool = AtomicBool::new(false);

macro_rules! log {
    ($($a:tt)*) => { log_line(&format!($($a)*)) };
}

fn log_line(s: &str) {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {s}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    // Steam's debug-output callback may come from its own threads: one lock for both sinks.
    let mut g = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
    if let Some(f) = g.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn cbuf(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

// ------------------------------------------------------------------ FFI types

const IDENTITY_STEAMID: i32 = 16; // k_ESteamNetworkingIdentityType_SteamID
const SEND_FLAGS: i32 = 0 | 1 | 32; // Unreliable | NoNagle | AutoRestartBrokenSession
const QUERY_PORT_SHARED: u16 = 0xffff; // MASTERSERVERUPDATERPORT_USEGAMESOCKETSHARE
const CHANNEL: i32 = 0;

/// SteamNetworkingIdentity, 136 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Identity {
    typ: i32,
    cb: i32,
    steam_id: u64,
    rest: [u8; 120],
}

impl Identity {
    fn steam(id: u64) -> Identity {
        Identity { typ: IDENTITY_STEAMID, cb: 8, steam_id: id, rest: [0; 120] }
    }
}

/// SteamNetworkingMessage_t, 216 bytes.
#[repr(C)]
struct Msg {
    data: *mut c_void,
    size: i32,
    conn: u32,
    peer: Identity,
    conn_user_data: i64,
    usec_received: i64,
    message_number: i64,
    free_data: *const c_void,
    release: *const c_void,
    channel: i32,
    flags: i32,
    user_data: i64,
    lane: u16,
    pad: u16,
}

/// SteamNetConnectionInfo_t, 696 bytes.
#[repr(C)]
struct ConnInfo {
    peer: Identity,
    user_data: i64,
    listen_socket: u32,
    addr: [u8; 18],
    pad1: u16,
    pop_remote: u32,
    pop_relay: u32,
    state: i32,
    end_reason: i32,
    end_debug: [u8; 128],
    description: [u8; 128],
    flags: i32,
    reserved: [u32; 63],
}

/// CallbackMsg_t (pack 8 on win64), 24 bytes.
#[repr(C)]
struct CallbackMsg {
    user: i32,
    id: i32,
    param: *mut u8,
    size: i32,
}

/// SteamRelayNetworkStatus_t, 272 bytes.
#[repr(C)]
struct RelayStatus {
    avail: i32,
    ping_in_progress: i32,
    avail_config: i32,
    avail_any_relay: i32,
    debug: [u8; 256],
}

/// SteamNetAuthenticationStatus_t, 260 bytes.
#[repr(C)]
struct AuthStatus {
    avail: i32,
    debug: [u8; 256],
}

/// SteamIPAddress_t (pack 1), 20 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IpAddr {
    v: [u8; 16],
    typ: i32,
}

/// SteamNetConnectionRealTimeStatus_t, 120 bytes.
#[repr(C)]
struct RtStatus {
    state: i32,
    ping: i32,
    quality_local: f32,
    quality_remote: f32,
    out_pps: f32,
    out_bps: f32,
    in_pps: f32,
    in_bps: f32,
    send_rate: i32,
    pending_unreliable: i32,
    pending_reliable: i32,
    sent_unacked_reliable: i32,
    usec_queue: i64,
    usec_max_jitter: i32,
    reserved: [u32; 15],
}

// ESteamNetworkingConfigValue / Scope / DataType (steamnetworkingtypes.h).
const CFG_SEND_RATE_MIN: i32 = 10;
const CFG_SEND_RATE_MAX: i32 = 11;
const CFG_SCOPE_GLOBAL: i32 = 1;
const CFG_ICE_ENABLE: i32 = 104; // P2P_Transport_ICE_Enable; values k_nSteamNetworkingConfig_P2P_Transport_ICE_Enable_*

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(offset_of!(RtStatus, send_rate) == 32);
    assert!(offset_of!(RtStatus, usec_queue) == 48);
    assert!(size_of::<RtStatus>() == 120);
    assert!(size_of::<Identity>() == 136);
    assert!(offset_of!(Msg, peer) == 16);
    assert!(offset_of!(Msg, channel) == 192);
    assert!(size_of::<Msg>() == 216);
    assert!(offset_of!(ConnInfo, state) == 176);
    assert!(offset_of!(ConnInfo, end_debug) == 184);
    assert!(offset_of!(ConnInfo, description) == 312);
    assert!(size_of::<ConnInfo>() == 696);
    assert!(offset_of!(CallbackMsg, param) == 8);
    assert!(size_of::<CallbackMsg>() == 24);
    assert!(size_of::<RelayStatus>() == 272);
    assert!(size_of::<AuthStatus>() == 260);
    assert!(size_of::<IpAddr>() == 20);
};

fn zeroed<T>() -> T {
    // SAFETY (all callers): plain C data, all-zero is valid.
    unsafe { std::mem::zeroed() }
}

// ------------------------------------------------------------------ flat API

type P = *mut c_void;
type ErrMsg = [u8; 1024]; // SteamErrMsg, k_cchMaxSteamErrMsg
type DebugFn = unsafe extern "C" fn(i32, *const c_char);

/// Function pointers from steam_api64.dll (signatures: steam_api_flat.h,
/// steam_api.h, steam_gameserver.h). C++ bool is a byte: taken as u8.
struct Api {
    md_init: unsafe extern "C" fn(),
    md_run_frame: unsafe extern "C" fn(i32),
    md_next: unsafe extern "C" fn(i32, *mut CallbackMsg) -> u8,
    md_free: unsafe extern "C" fn(i32),
    // game server
    gs_init: unsafe extern "C" fn(u32, u16, u16, i32, *const c_char, *const c_char, *mut ErrMsg) -> i32,
    gs_pipe: unsafe extern "C" fn() -> i32,
    gs_shutdown: unsafe extern "C" fn(),
    gs: unsafe extern "C" fn() -> P,
    gs_msgs: unsafe extern "C" fn() -> P,
    gs_sockets: unsafe extern "C" fn() -> P,
    gs_set_product: unsafe extern "C" fn(P, *const c_char),
    gs_set_moddir: unsafe extern "C" fn(P, *const c_char),
    gs_set_dedicated: unsafe extern "C" fn(P, u8),
    gs_logon_anon: unsafe extern "C" fn(P),
    gs_logon: unsafe extern "C" fn(P, *const c_char),
    gs_logoff: unsafe extern "C" fn(P),
    gs_logged_on: unsafe extern "C" fn(P) -> u8,
    gs_secure: unsafe extern "C" fn(P) -> u8,
    gs_steamid: unsafe extern "C" fn(P) -> u64,
    gs_public_ip: unsafe extern "C" fn(P) -> IpAddr,
    // client
    api_init: unsafe extern "C" fn(*mut ErrMsg) -> i32,
    api_pipe: unsafe extern "C" fn() -> i32,
    api_shutdown: unsafe extern "C" fn(),
    user: unsafe extern "C" fn() -> P,
    user_logged_on: unsafe extern "C" fn(P) -> u8,
    user_steamid: unsafe extern "C" fn(P) -> u64,
    msgs: unsafe extern "C" fn() -> P,
    sockets: unsafe extern "C" fn() -> P,
    // shared
    utils: unsafe extern "C" fn() -> P,
    init_relay: unsafe extern "C" fn(P),
    relay_status: unsafe extern "C" fn(P, *mut RelayStatus) -> i32,
    ping_loc: unsafe extern "C" fn(P, *mut [u8; 512]) -> f32,
    ping_str: unsafe extern "C" fn(P, *const [u8; 512], *mut c_char, i32),
    ping_up_to_date: unsafe extern "C" fn(P, f32) -> u8,
    set_debug: unsafe extern "C" fn(P, i32, Option<DebugFn>),
    auth_status: unsafe extern "C" fn(P, *mut AuthStatus) -> i32,
    init_auth: unsafe extern "C" fn(P) -> i32,
    send: unsafe extern "C" fn(P, *const Identity, *const c_void, u32, i32, i32) -> i32,
    recv: unsafe extern "C" fn(P, i32, *mut *mut Msg, i32) -> i32,
    accept: unsafe extern "C" fn(P, *const Identity) -> u8,
    close: unsafe extern "C" fn(P, *const Identity) -> u8,
    conn_info: unsafe extern "C" fn(P, *const Identity, *mut ConnInfo, *mut RtStatus) -> i32,
    release: unsafe extern "C" fn(*mut Msg),
    set_cfg_i32: unsafe extern "C" fn(P, i32, i32) -> u8,
    get_cfg: unsafe extern "C" fn(P, i32, i32, isize, *mut i32, *mut c_void, *mut usize) -> i32,
    pop_count: unsafe extern "C" fn(P) -> i32,
    pop_list: unsafe extern "C" fn(P, *mut u32, i32) -> i32,
    ping_dc: unsafe extern "C" fn(P, u32, *mut u32) -> i32,
}

/// GetProcAddress by name into fn-pointer type F.
unsafe fn sym<F: Copy>(m: windows_sys::Win32::Foundation::HMODULE, name: &str) -> Result<F, String> {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
    let c = format!("{name}\0");
    // SAFETY: m is loaded, c NUL-terminated; F has the export's signature (caller).
    match unsafe { GetProcAddress(m, c.as_ptr()) } {
        Some(f) => {
            log!("  resolved {name} @ {:p}", f as *const c_void);
            Ok(unsafe { std::mem::transmute_copy(&f) })
        }
        None => Err(format!("steam_api64.dll lacks export {name}")),
    }
}

fn load_api(dir: &std::path::Path) -> Result<Api, String> {
    let path = dir.join("steam_api64.dll");
    let w: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
    // SAFETY: w NUL-terminated.
    let m = unsafe { LoadLibraryW(w.as_ptr()) };
    if m.is_null() {
        return Err(format!("LoadLibraryW({}) failed: {}", path.display(), std::io::Error::last_os_error()));
    }
    log!("LoadLibraryW({}) = {:p}", path.display(), m);
    macro_rules! s {
        ($n:literal) => {
            unsafe { sym(m, $n)? }
        };
    }
    Ok(Api {
        md_init: s!("SteamAPI_ManualDispatch_Init"),
        md_run_frame: s!("SteamAPI_ManualDispatch_RunFrame"),
        md_next: s!("SteamAPI_ManualDispatch_GetNextCallback"),
        md_free: s!("SteamAPI_ManualDispatch_FreeLastCallback"),
        gs_init: s!("SteamInternal_GameServer_Init_V2"),
        gs_pipe: s!("SteamGameServer_GetHSteamPipe"),
        gs_shutdown: s!("SteamGameServer_Shutdown"),
        gs: s!("SteamAPI_SteamGameServer_v015"),
        gs_msgs: s!("SteamAPI_SteamGameServerNetworkingMessages_SteamAPI_v002"),
        // Factorio's steam_api64.dll has only v013; the two methods used here are the same in both.
        gs_sockets: unsafe {
            sym(m, "SteamAPI_SteamGameServerNetworkingSockets_SteamAPI_v012")
                .or_else(|_| sym(m, "SteamAPI_SteamGameServerNetworkingSockets_SteamAPI_v013"))?
        },
        gs_set_product: s!("SteamAPI_ISteamGameServer_SetProduct"),
        gs_set_moddir: s!("SteamAPI_ISteamGameServer_SetModDir"),
        gs_set_dedicated: s!("SteamAPI_ISteamGameServer_SetDedicatedServer"),
        gs_logon_anon: s!("SteamAPI_ISteamGameServer_LogOnAnonymous"),
        gs_logon: s!("SteamAPI_ISteamGameServer_LogOn"),
        gs_logoff: s!("SteamAPI_ISteamGameServer_LogOff"),
        gs_logged_on: s!("SteamAPI_ISteamGameServer_BLoggedOn"),
        gs_secure: s!("SteamAPI_ISteamGameServer_BSecure"),
        gs_steamid: s!("SteamAPI_ISteamGameServer_GetSteamID"),
        gs_public_ip: s!("SteamAPI_ISteamGameServer_GetPublicIP"),
        api_init: s!("SteamAPI_InitFlat"),
        api_pipe: s!("SteamAPI_GetHSteamPipe"),
        api_shutdown: s!("SteamAPI_Shutdown"),
        user: s!("SteamAPI_SteamUser_v023"),
        user_logged_on: s!("SteamAPI_ISteamUser_BLoggedOn"),
        user_steamid: s!("SteamAPI_ISteamUser_GetSteamID"),
        msgs: s!("SteamAPI_SteamNetworkingMessages_SteamAPI_v002"),
        sockets: unsafe {
            sym(m, "SteamAPI_SteamNetworkingSockets_SteamAPI_v012")
                .or_else(|_| sym(m, "SteamAPI_SteamNetworkingSockets_SteamAPI_v013"))?
        },
        utils: s!("SteamAPI_SteamNetworkingUtils_SteamAPI_v004"),
        init_relay: s!("SteamAPI_ISteamNetworkingUtils_InitRelayNetworkAccess"),
        relay_status: s!("SteamAPI_ISteamNetworkingUtils_GetRelayNetworkStatus"),
        ping_loc: s!("SteamAPI_ISteamNetworkingUtils_GetLocalPingLocation"),
        ping_str: s!("SteamAPI_ISteamNetworkingUtils_ConvertPingLocationToString"),
        ping_up_to_date: s!("SteamAPI_ISteamNetworkingUtils_CheckPingDataUpToDate"),
        set_debug: s!("SteamAPI_ISteamNetworkingUtils_SetDebugOutputFunction"),
        auth_status: s!("SteamAPI_ISteamNetworkingSockets_GetAuthenticationStatus"),
        init_auth: s!("SteamAPI_ISteamNetworkingSockets_InitAuthentication"),
        send: s!("SteamAPI_ISteamNetworkingMessages_SendMessageToUser"),
        recv: s!("SteamAPI_ISteamNetworkingMessages_ReceiveMessagesOnChannel"),
        accept: s!("SteamAPI_ISteamNetworkingMessages_AcceptSessionWithUser"),
        close: s!("SteamAPI_ISteamNetworkingMessages_CloseSessionWithUser"),
        conn_info: s!("SteamAPI_ISteamNetworkingMessages_GetSessionConnectionInfo"),
        release: s!("SteamAPI_SteamNetworkingMessage_t_Release"),
        set_cfg_i32: s!("SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueInt32"),
        get_cfg: s!("SteamAPI_ISteamNetworkingUtils_GetConfigValue"),
        pop_count: s!("SteamAPI_ISteamNetworkingUtils_GetPOPCount"),
        pop_list: s!("SteamAPI_ISteamNetworkingUtils_GetPOPList"),
        ping_dc: s!("SteamAPI_ISteamNetworkingUtils_GetPingToDataCenter"),
    })
}

// ------------------------------------------------------------------ decoders

fn avail_name(a: i32) -> &'static str {
    match a {
        -102 => "CannotTry",
        -101 => "Failed",
        -100 => "Previously",
        -10 => "Retrying",
        0 => "Unknown",
        1 => "NeverTried",
        2 => "Waiting",
        3 => "Attempting",
        100 => "Current",
        _ => "?",
    }
}

fn state_name(s: i32) -> &'static str {
    match s {
        0 => "None",
        1 => "Connecting",
        2 => "FindingRoute",
        3 => "Connected",
        4 => "ClosedByPeer",
        5 => "ProblemDetectedLocally",
        -1 => "FinWait",
        -2 => "Linger",
        -3 => "Dead",
        _ => "?",
    }
}

fn init_result_name(r: i32) -> &'static str {
    match r {
        0 => "OK",
        1 => "FailedGeneric",
        2 => "NoSteamClient",
        3 => "VersionMismatch",
        _ => "?",
    }
}

fn eres(r: i32) -> String {
    format!("{r} ({})", names::eresult(r))
}

/// SteamID64 bits: account 0..32, instance 32..52, type 52..56, universe 56..64.
fn steamid_desc(id: u64) -> String {
    let typ = match (id >> 52) & 0xf {
        0 => "Invalid",
        1 => "Individual",
        2 => "Multiseat",
        3 => "GameServer",
        4 => "AnonGameServer",
        5 => "Pending",
        6 => "ContentServer",
        7 => "Clan",
        8 => "Chat",
        9 => "ConsoleUser",
        10 => "AnonUser",
        _ => "?",
    };
    let uni = match id >> 56 {
        0 => "Invalid",
        1 => "Public",
        2 => "Beta",
        3 => "Internal",
        4 => "Dev",
        _ => "?",
    };
    format!(
        "{id} (0x{id:016x} type={typ} universe={uni} account={} instance={})",
        id & 0xffff_ffff,
        (id >> 32) & 0xf_ffff
    )
}

fn steamid_valid(id: u64) -> bool {
    (id >> 52) & 0xf != 0 && id >> 56 != 0 && id & 0xffff_ffff != 0
}

/// SteamNetworkingPOPID -> code, as GetSteamNetworkingLocationPOPStringFromID.
fn pop_code(id: u32) -> String {
    [id >> 16, id >> 8, id, id >> 24].iter().map(|&b| b as u8).filter(|&b| b != 0).map(char::from).collect()
}

fn relay_desc(r: &RelayStatus) -> String {
    format!(
        "avail={} ({}) pingMeasurementInProgress={} networkConfig={} ({}) anyRelay={} ({}) debug='{}'",
        r.avail,
        avail_name(r.avail),
        r.ping_in_progress,
        r.avail_config,
        avail_name(r.avail_config),
        r.avail_any_relay,
        avail_name(r.avail_any_relay),
        cbuf(&r.debug)
    )
}

fn conn_desc(i: &ConnInfo) -> String {
    format!(
        "peer={} state={} ({}) endReason={} ({}) endDebug='{}' popRemote={} popRelay={} flags=0x{:x} [{}] desc='{}'",
        i.peer.steam_id,
        i.state,
        state_name(i.state),
        i.end_reason,
        names::end_reason(i.end_reason),
        cbuf(&i.end_debug),
        pop_code(i.pop_remote),
        pop_code(i.pop_relay),
        i.flags,
        flags_desc(i.flags),
        cbuf(&i.description)
    )
}

/// k_nSteamNetworkConnectionInfoFlags_* (steamnetworkingtypes.h).
fn flags_desc(f: i32) -> String {
    const N: [&str; 6] = ["Unauthenticated", "Unencrypted", "LoopbackBuffers", "Fast", "Relayed", "DualWifi"];
    let v: Vec<&str> = N.iter().enumerate().filter(|(b, _)| f & (1 << b) != 0).map(|(_, n)| *n).collect();
    if f & 16 == 0 { format!("{} direct", v.join(",")) } else { v.join(",") }
}

unsafe extern "C" fn steam_debug(typ: i32, msg: *const c_char) {
    let t = match typ {
        1 => "Bug",
        2 => "Error",
        3 => "Important",
        4 => "Warning",
        5 => "Msg",
        6 => "Verbose",
        7 => "Debug",
        8 => "Everything",
        _ => "?",
    };
    let s = if msg.is_null() {
        String::new()
    } else {
        // SAFETY: Steam passes a NUL-terminated string.
        unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned()
    };
    log!("steamnet[{t}] {}", s.trim_end());
}

static DONE: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(t: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    // CTRL_CLOSE_EVENT (console window closed): the process dies when this returns, so give run() time to shut Steam down.
    if t == 2 {
        for _ in 0..80 {
            if DONE.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    1
}

/// play mode: the game command, launched at once (or, with --connect, once the client tunnel is READY / after 20 s).
struct Play {
    cmd: Vec<String>,
    /// Steam env as we got it, restored for the game (we overwrite it for our own SteamAPI init).
    env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    start: Instant,
    /// --connect: wait for READY so the game's --mp-connect finds a working tunnel.
    wait: bool,
    child: Option<std::process::Child>,
    code: Option<i32>,
}

impl Play {
    /// true = stop the tunnel (game exited or could not start).
    fn poll(&mut self, ready: bool) -> bool {
        let r = match &mut self.child {
            None => {
                if self.wait && !ready && self.start.elapsed() < Duration::from_secs(20) {
                    return false;
                }
                if self.wait && !ready {
                    log!("WARNING: tunnel not READY after 20 s; launching the game anyway");
                }
                let mut c = std::process::Command::new(&self.cmd[0]);
                c.args(&self.cmd[1..]);
                for (k, v) in &self.env {
                    match v {
                        Some(v) => c.env(k, v),
                        None => c.env_remove(k),
                    };
                }
                log!("spawning game: {:?}", self.cmd);
                match c.spawn() {
                    Ok(ch) => {
                        log!("game pid {}", ch.id());
                        self.child = Some(ch);
                        return false;
                    }
                    Err(e) => Err(format!("spawn failed: {e}")),
                }
            }
            Some(ch) => match ch.try_wait() {
                Ok(None) => return false,
                Ok(Some(s)) => Ok(s),
                Err(e) => Err(format!("wait failed: {e}")),
            },
        };
        match r {
            Ok(s) => {
                log!("game exited: {s}");
                self.code = Some(s.code().unwrap_or(1));
            }
            Err(e) => {
                log!("FATAL: game {e}");
                self.code = Some(1);
            }
        }
        true
    }
}

// ------------------------------------------------------------------ tunnel state

struct Counters {
    rx_pkts: u64,
    rx_bytes: u64,
    tx_pkts: u64,
    tx_bytes: u64,
    last_send_err: i32,
}

impl Counters {
    fn new() -> Counters {
        Counters { rx_pkts: 0, rx_bytes: 0, tx_pkts: 0, tx_bytes: 0, last_send_err: 1 }
    }
}

/// One remote Steam peer of the host.
struct Peer {
    sock: UdpSocket,
    last: Instant,
    c: Counters,
}

enum Role {
    Host { target: SocketAddr, idle: Duration, peers: HashMap<u64, Peer> },
    Client { host: u64, sock: UdpSocket, local: Option<SocketAddr>, c: Counters },
}

struct Ctx {
    api: Api,
    pipe: i32,
    is_host: bool,
    gs: P,
    user: P,
    msgs: P,
    sockets: P,
    utils: P,
    ready: bool,
    role: Role,
    play: Option<Play>,
    /// last logged (Relayed flag, relay POP) per session, to log route changes.
    routes: std::cell::RefCell<HashMap<u64, (i32, u32)>>,
}

impl Ctx {
    fn send(&self, to: u64, data: &[u8], c: &mut Counters) {
        let id = Identity::steam(to);
        // SAFETY: msgs live, id/data valid for the call.
        let r = unsafe {
            (self.api.send)(self.msgs, &id, data.as_ptr().cast(), data.len() as u32, SEND_FLAGS, CHANNEL)
        };
        if r == 1 {
            if c.tx_pkts == 0 {
                log!("first datagram UDP->SDR to {to}: {} bytes", data.len());
            }
            c.tx_pkts += 1;
            c.tx_bytes += data.len() as u64;
        }
        if r != c.last_send_err {
            log!("SendMessageToUser({to}, {} bytes) = {}", data.len(), eres(r));
            c.last_send_err = r;
        }
    }

    fn on_callback(&mut self, cb: &CallbackMsg) {
        let name = names::callback(cb.id);
        let p = cb.param;
        let size = cb.size.max(0) as usize;
        // SAFETY (field reads below): Steam's param is valid for `size` bytes; each read is size-checked.
        let rd_i32 = |off: usize| -> i32 {
            if off + 4 <= size { unsafe { p.add(off).cast::<i32>().read_unaligned() } } else { i32::MIN }
        };
        let detail = match cb.id {
            101 => String::new(),
            102 => format!("eResult={} bStillRetrying={}", eres(rd_i32(0)), if size > 4 { unsafe { *p.add(4) } } else { 255 }),
            103 => format!("eResult={}", eres(rd_i32(0))),
            115 => format!("bSecure={}", if size >= 1 { unsafe { *p } } else { 255 }),
            703 if size >= 16 => format!(
                "hAsyncCall={} iCallback={} ({}) cubParam={}",
                unsafe { p.cast::<u64>().read_unaligned() },
                rd_i32(8),
                names::callback(rd_i32(8)),
                rd_i32(12)
            ),
            1222 if size >= std::mem::size_of::<AuthStatus>() => {
                let a = unsafe { p.cast::<AuthStatus>().read_unaligned() };
                format!("avail={} ({}) debug='{}'", a.avail, avail_name(a.avail), cbuf(&a.debug))
            }
            1281 if size >= std::mem::size_of::<RelayStatus>() => {
                relay_desc(&unsafe { p.cast::<RelayStatus>().read_unaligned() })
            }
            1252 if size >= std::mem::size_of::<ConnInfo>() => {
                conn_desc(&unsafe { p.cast::<ConnInfo>().read_unaligned() })
            }
            1221 if size >= 8 + std::mem::size_of::<ConnInfo>() => {
                // SteamNetConnectionStatusChangedCallback_t: hConn, info @8, eOldState after it.
                let i = unsafe { p.add(8).cast::<ConnInfo>().read_unaligned() };
                format!("hConn={} {} oldState={}", rd_i32(0), conn_desc(&i), rd_i32(8 + 696))
            }
            1251 if size >= 136 => {
                let id = unsafe { p.cast::<Identity>().read_unaligned() };
                // SAFETY: msgs live, id valid.
                let ok = unsafe { (self.api.accept)(self.msgs, &id) };
                format!("identityRemote type={} steamid={} -> AcceptSessionWithUser = {ok}", id.typ, steamid_desc(id.steam_id))
            }
            _ => {
                let n = size.min(32);
                let bytes: Vec<String> = (0..n).map(|i| format!("{:02x}", unsafe { *p.add(i) })).collect();
                format!("raw[{n}/{size}]={}", bytes.join(" "))
            }
        };
        log!("CALLBACK id={} {name} user={} size={size} {detail}", cb.id, cb.user);
    }

    fn pump_callbacks(&mut self) -> bool {
        // SAFETY: pipe from GetHSteamPipe; manual dispatch initialised.
        unsafe { (self.api.md_run_frame)(self.pipe) };
        let mut any = false;
        loop {
            let mut cb: CallbackMsg = zeroed();
            // SAFETY: as above, cb writable.
            if unsafe { (self.api.md_next)(self.pipe, &mut cb) } == 0 {
                break;
            }
            any = true;
            self.on_callback(&cb);
            // SAFETY: GetNextCallback returned true.
            unsafe { (self.api.md_free)(self.pipe) };
        }
        any
    }

    /// SDR -> UDP.
    fn pump_sdr(&mut self) -> bool {
        let mut ptrs = [std::ptr::null_mut::<Msg>(); 64];
        // SAFETY: msgs live, ptrs has room for 64.
        let n = unsafe { (self.api.recv)(self.msgs, CHANNEL, ptrs.as_mut_ptr(), 64) };
        for &m in &ptrs[..n.max(0) as usize] {
            // SAFETY: Steam handed n valid messages; released right after.
            let (from, data) = unsafe {
                let msg = &*m;
                (msg.peer.steam_id, std::slice::from_raw_parts(msg.data as *const u8, msg.size.max(0) as usize).to_vec())
            };
            unsafe { (self.api.release)(m) };
            match &mut self.role {
                Role::Host { target, peers, .. } => {
                    let peer = match peers.entry(from) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let sock = match UdpSocket::bind("127.0.0.1:0").and_then(|s| s.set_nonblocking(true).map(|()| s)) {
                                Ok(s) => s,
                                Err(e) => {
                                    log!("peer {from}: local UDP bind failed: {e}; datagram dropped");
                                    continue;
                                }
                            };
                            log!(
                                "NEW PEER {}: local socket {} -> target {target}",
                                steamid_desc(from),
                                sock.local_addr().map(|a| a.to_string()).unwrap_or_default()
                            );
                            e.insert(Peer { sock, last: Instant::now(), c: Counters::new() })
                        }
                    };
                    peer.last = Instant::now();
                    if peer.c.rx_pkts == 0 {
                        log!("first datagram SDR->UDP from {from}: {} bytes", data.len());
                    }
                    peer.c.rx_pkts += 1;
                    peer.c.rx_bytes += data.len() as u64;
                    if let Err(e) = peer.sock.send_to(&data, *target) {
                        log!("peer {from}: send_to {target} failed: {e}");
                    }
                }
                Role::Client { host, sock, local, c } => {
                    if from != *host {
                        log!("ignoring {} bytes from unexpected peer {from}", data.len());
                        continue;
                    }
                    if c.rx_pkts == 0 {
                        log!("first datagram SDR->UDP from host: {} bytes", data.len());
                    }
                    c.rx_pkts += 1;
                    c.rx_bytes += data.len() as u64;
                    match local {
                        Some(a) => {
                            if let Err(e) = sock.send_to(&data, *a) {
                                log!("send_to local {a} failed: {e}");
                            }
                        }
                        None => log!("{} bytes from host but no local UDP sender seen yet; dropped", data.len()),
                    }
                }
            }
        }
        n > 0
    }

    /// UDP -> SDR.
    fn pump_udp(&mut self) -> bool {
        let mut buf = [0u8; 65536];
        let mut any = false;
        // Out-of-borrow sends: collect then send.
        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        match &mut self.role {
            Role::Host { peers, .. } => {
                for (&id, p) in peers.iter_mut() {
                    loop {
                        match p.sock.recv_from(&mut buf) {
                            Ok((n, _)) => out.push((id, buf[..n].to_vec())),
                            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                            // Windows reports an ICMP port-unreachable from the target as ConnectionReset.
                            Err(e) => {
                                log!("peer {id}: local recv error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            Role::Client { host, sock, local, .. } => loop {
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        if *local != Some(src) {
                            log!("local UDP sender is now {src}");
                            *local = Some(src);
                        }
                        out.push((*host, buf[..n].to_vec()));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log!("listen socket recv error: {e}");
                        break;
                    }
                }
            },
        }
        for (id, data) in out {
            any = true;
            let mut c = match &mut self.role {
                Role::Host { peers, .. } => std::mem::replace(&mut peers.get_mut(&id).unwrap().c, Counters::new()),
                Role::Client { c, .. } => std::mem::replace(c, Counters::new()),
            };
            self.send(id, &data, &mut c);
            match &mut self.role {
                Role::Host { peers, .. } => peers.get_mut(&id).unwrap().c = c,
                Role::Client { c: cc, .. } => *cc = c,
            }
        }
        any
    }

    fn reap_idle(&mut self) {
        let Role::Host { peers, idle, .. } = &mut self.role else { return };
        let dead: Vec<u64> = peers.iter().filter(|(_, p)| p.last.elapsed() > *idle).map(|(&id, _)| id).collect();
        for id in dead {
            let p = peers.remove(&id).unwrap();
            let ident = Identity::steam(id);
            // SAFETY: msgs live.
            let ok = unsafe { (self.api.close)(self.msgs, &ident) };
            log!(
                "peer {id} idle > {} s: removed (rx {} pkts/{} B, tx {} pkts/{} B), CloseSessionWithUser = {ok}",
                idle.as_secs(),
                p.c.rx_pkts,
                p.c.rx_bytes,
                p.c.tx_pkts,
                p.c.tx_bytes
            );
        }
    }

    fn sessions_diag(&self) {
        let list: Vec<(u64, &Counters)> = match &self.role {
            Role::Host { peers, .. } => peers.iter().map(|(&id, p)| (id, &p.c)).collect(),
            Role::Client { host, c, .. } => vec![(*host, c)],
        };
        for (id, c) in list {
            let ident = Identity::steam(id);
            let mut info: ConnInfo = zeroed();
            let mut rt: RtStatus = zeroed();
            // SAFETY: msgs live; info and rt writable.
            let s = unsafe { (self.api.conn_info)(self.msgs, &ident, &mut info, &mut rt) };
            log!(
                "SESSION {id}: GetSessionConnectionInfo={s} ({}) {} | rx {} pkts/{} B, tx {} pkts/{} B",
                state_name(s),
                conn_desc(&info),
                c.rx_pkts,
                c.rx_bytes,
                c.tx_pkts,
                c.tx_bytes
            );
            let route = (info.flags & 16, info.pop_relay);
            if s == 3 && self.routes.borrow_mut().insert(id, route) != Some(route) {
                log!(
                    "ROUTE {id}: {} relayPOP={} remotePOP={} addr/desc='{}'",
                    if route.0 != 0 { "RELAYED" } else { "DIRECT" },
                    pop_code(info.pop_relay),
                    pop_code(info.pop_remote),
                    cbuf(&info.description)
                );
            }
            log!(
                "RT {id}: ping={} ms quality local={:.3} remote={:.3} out={:.0} pkt/s {:.0} B/s in={:.0} pkt/s {:.0} B/s sendRate={} B/s pendingUnrel={} B pendingRel={} B queue={} us jitterMax={} us",
                rt.ping,
                rt.quality_local,
                rt.quality_remote,
                rt.out_pps,
                rt.out_bps,
                rt.in_pps,
                rt.in_bps,
                rt.send_rate,
                rt.pending_unreliable,
                rt.pending_reliable,
                rt.usec_queue,
                rt.usec_max_jitter
            );
        }
    }

    /// Top 10 relay POPs by ping (GetPingToDataCenter: best route, maybe via another POP).
    fn pop_pings(&self) {
        // SAFETY (whole fn): utils live; list sized to the count we pass.
        let n = unsafe { (self.api.pop_count)(self.utils) }.max(0);
        let mut ids = vec![0u32; n as usize];
        let got = unsafe { (self.api.pop_list)(self.utils, ids.as_mut_ptr(), n) }.clamp(0, n) as usize;
        let mut pings: Vec<(i32, u32, u32)> = ids[..got]
            .iter()
            .map(|&id| {
                let mut via = 0u32;
                (unsafe { (self.api.ping_dc)(self.utils, id, &mut via) }, id, via)
            })
            .filter(|p| p.0 >= 0)
            .collect();
        pings.sort();
        let top: Vec<String> = pings
            .iter()
            .take(10)
            .map(|&(ms, id, via)| if via == 0 || via == id { format!("{} {ms}ms", pop_code(id)) } else { format!("{} {ms}ms via {}", pop_code(id), pop_code(via)) })
            .collect();
        log!("POPs: {got} listed, {} with ping; best: {}", pings.len(), top.join(", "));
    }

    fn diag(&mut self) {
        let a = &self.api;
        // SAFETY (whole fn): the interface pointers are live after init.
        let (logged, sid) = unsafe {
            if self.is_host {
                let logged = (a.gs_logged_on)(self.gs);
                let sid = (a.gs_steamid)(self.gs);
                let ip = (a.gs_public_ip)(self.gs);
                let (v, typ) = (ip.v, ip.typ);
                let ip_s = if typ == 0 {
                    std::net::Ipv4Addr::from(u32::from_le_bytes([v[0], v[1], v[2], v[3]])).to_string()
                } else {
                    format!("{:02x?}", v)
                };
                log!(
                    "DIAG gameserver: BLoggedOn={logged} BSecure={} GetSteamID={} GetPublicIP={ip_s} (type {typ})",
                    (a.gs_secure)(self.gs),
                    steamid_desc(sid)
                );
                (logged, sid)
            } else {
                let logged = (a.user_logged_on)(self.user);
                let sid = (a.user_steamid)(self.user);
                log!("DIAG user: BLoggedOn={logged} GetSteamID={}", steamid_desc(sid));
                (logged, sid)
            }
        };
        let mut rs: RelayStatus = zeroed();
        let relay = unsafe { (a.relay_status)(self.utils, &mut rs) };
        log!("DIAG GetRelayNetworkStatus={relay} ({}) {}", avail_name(relay), relay_desc(&rs));
        let mut au: AuthStatus = zeroed();
        let auth = unsafe { (a.auth_status)(self.sockets, &mut au) };
        log!("DIAG GetAuthenticationStatus={auth} ({}) debug='{}'", avail_name(auth), cbuf(&au.debug));
        let mut loc = [0u8; 512];
        let age = unsafe { (a.ping_loc)(self.utils, &mut loc) };
        let mut s = [0u8; 1024]; // k_cchMaxSteamNetworkingPingLocationString
        if age >= 0.0 {
            unsafe { (a.ping_str)(self.utils, &loc, s.as_mut_ptr().cast(), s.len() as i32) };
        }
        let fresh = unsafe { (a.ping_up_to_date)(self.utils, 300.0) };
        log!("DIAG GetLocalPingLocation age={age} s location='{}' CheckPingDataUpToDate(300)={fresh}", cbuf(&s));
        self.sessions_diag();
        let ready = logged != 0 && steamid_valid(sid) && relay == 100;
        if ready && !self.ready {
            log!("READY steamid={sid} relay=Current auth={} id={}", avail_name(auth), steamid_desc(sid));
            self.pop_pings();
        } else if !ready && self.ready {
            log!("NOT READY any more (logged={logged} steamid={sid} relay={})", avail_name(relay));
        }
        self.ready = ready;
    }

    fn run(&mut self) {
        let start = Instant::now();
        let mut next_diag = Instant::now();
        let mut next_sess = Instant::now() + Duration::from_secs(5);
        while !STOP.load(Ordering::SeqCst) {
            let mut busy = self.pump_callbacks();
            busy |= self.pump_sdr();
            busy |= self.pump_udp();
            let now = Instant::now();
            if now >= next_diag {
                log!("--- diag t+{:.1} s", start.elapsed().as_secs_f32());
                self.diag();
                next_diag = now + Duration::from_secs(if self.ready { 30 } else { 2 });
                next_sess = now + Duration::from_secs(5);
            } else if now >= next_sess {
                self.sessions_diag();
                next_sess = now + Duration::from_secs(5);
            }
            self.reap_idle();
            if let Some(p) = &mut self.play {
                if p.poll(self.ready) {
                    STOP.store(true, Ordering::SeqCst);
                }
            }
            if !busy {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        log!("stop requested");
        unsafe {
            (self.api.set_debug)(self.utils, 0, None);
            if self.is_host {
                (self.api.gs_logoff)(self.gs);
                log!("LogOff called");
                for _ in 0..20 {
                    self.pump_callbacks();
                    std::thread::sleep(Duration::from_millis(50));
                }
                (self.api.gs_shutdown)();
                log!("SteamGameServer_Shutdown called");
            } else {
                (self.api.api_shutdown)();
                log!("SteamAPI_Shutdown called");
            }
        }
        DONE.store(true, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ main

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

const DEFAULT_APPID: u64 = 480; // 427520 (Factorio) also works for SDR, but the GSLT in use is for 480

const USAGE: &str = "usage:\n  sdr host   [--appid 480] [--target 127.0.0.1:34197] [--game-port 27016] [--mode 2] [--idle 60] [--debug-level 5] [--gslt-file <path>]\n  sdr client [--appid 480] --host <SteamID64> [--listen 127.0.0.1:34200] [--debug-level 5]\n  sdr play   [--appid 480] --host <SteamID64> [--port 34200] [--connect] [--debug-level 5] -- <game exe> [game args...]\n\
     play: client tunnel on 127.0.0.1:<port> (busy -> a free port, logged), runs the game, exits with the game's exit code;\n\
           --connect appends --mp-connect 127.0.0.1:<port> to the game command; log sdr.log next to the exe (overwritten)\n\
     all roles: [--send-rate-min 1000000] [--send-rate-max 10000000] (bytes/s, Steam default 256 KB/s) [--ice all|private|public|off]\n\
     --mode: EServerMode 1=NoAuthentication 2=Authentication 3=AuthenticationAndSecure\n\
     --debug-level: Steam networking debug output 0=None 2=Error 4=Warning 5=Msg 6=Verbose 7=Debug 8=Everything\n\
     --gslt-file: Game Server Login Token file (default: gslt.txt next to the exe if present; env SDR_GSLT also works);\n\
                  without a token the host logs on anonymously (new SteamID every run)";

/// Tunnel setup failed: exit 1, but in play mode still run the game (and wait for it) first.
fn die(play: &mut Option<Play>) -> ! {
    let Some(p) = play else { std::process::exit(1) };
    log!("tunnel unavailable; launching the game without it");
    p.wait = false;
    while !p.poll(false) {
        std::thread::sleep(Duration::from_millis(200));
    }
    std::process::exit(p.code.unwrap_or(1))
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

/// GSLT and where it came from: --gslt-file, else env SDR_GSLT, else gslt.txt next to the exe.
fn gslt(args: &[String]) -> Result<Option<(String, String)>, String> {
    let (tok, src) = if let Some(p) = opt(args, "--gslt-file") {
        (std::fs::read_to_string(&p).map_err(|e| format!("cannot read --gslt-file {p}: {e}"))?, format!("file {p}"))
    } else if let Ok(t) = std::env::var("SDR_GSLT") {
        (t, "env SDR_GSLT".into())
    } else {
        let p = exe_dir().join("gslt.txt");
        match std::fs::read_to_string(&p) {
            Ok(t) => (t, format!("file {}", p.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
        }
    };
    let tok = tok.trim();
    Ok((!tok.is_empty()).then(|| (tok.to_string(), src)))
}

/// Token for the log: never the token itself.
fn mask(tok: &str) -> String {
    let n = tok.chars().count();
    let last4: String = if n >= 8 { tok.chars().skip(n - 4).collect() } else { String::new() };
    format!("token len={n}, ****{last4}")
}

fn main() {
    let all: Vec<String> = std::env::args().collect();
    // Our options end at "--"; the rest is the game command (play).
    let split = all.iter().position(|a| a == "--").unwrap_or(all.len());
    let args = &all[..split];
    let mut game: Vec<String> = all.get(split + 1..).unwrap_or_default().to_vec();
    if args.iter().skip(1).any(|a| a == "-h" || a == "--help" || a == "help") {
        println!("{USAGE}");
        return;
    }
    let role = args.get(1).map(String::as_str).unwrap_or("");
    let (is_host, is_play) = match role {
        "host" => (true, false),
        "client" => (false, false),
        "play" if !game.is_empty() => (false, true),
        _ => usage(),
    };
    let num = |name: &str, def: u64| -> u64 {
        opt(args, name).map_or(def, |v| v.parse().unwrap_or_else(|_| usage()))
    };
    let appid = num("--appid", DEFAULT_APPID);
    let debug_level = num("--debug-level", 5) as i32;
    let rate_min = num("--send-rate-min", 1_000_000).min(i32::MAX as u64) as i32;
    let rate_max = num("--send-rate-max", 10_000_000).min(i32::MAX as u64) as i32;
    let ice = match opt(args, "--ice").as_deref().unwrap_or("all") {
        "all" => 0x7fff_ffff,
        "private" => 2,
        "public" => 4,
        "off" => 0,
        _ => usage(),
    };

    // play: one log, overwritten per run (it lives in the game folder).
    let log_path = exe_dir().join(if is_play { "sdr.log".to_string() } else { format!("sdr-{role}.log") });
    match OpenOptions::new().create(true).write(true).append(!is_play).truncate(is_play).open(&log_path) {
        Ok(f) => *LOG.lock().unwrap() = Some(f),
        Err(e) => eprintln!("cannot open {}: {e}; logging to stdout only", log_path.display()),
    }
    log!("==== sdr {} {role} start, args {:?}, log {}", env!("CARGO_PKG_VERSION"), &all[1..], log_path.display());
    // SAFETY: handler only touches atomics and sleeps.
    let ok = unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    log!("SetConsoleCtrlHandler = {ok}");

    let steam_env: Vec<(&'static str, Option<std::ffi::OsString>)> =
        ["SteamAppId", "SteamGameId"].into_iter().map(|k| (k, std::env::var_os(k))).collect();
    log!("env before: {steam_env:?}");
    std::env::set_var("SteamAppId", appid.to_string());
    std::env::set_var("SteamGameId", appid.to_string());
    log!("env SteamAppId=SteamGameId={appid}");
    // play runs in the game folder: never touch its steam_appid.txt, env is enough there.
    if !is_play {
        let appid_file = exe_dir().join("steam_appid.txt");
        match std::fs::write(&appid_file, appid.to_string()) {
            Ok(()) => log!("wrote {} = {appid}", appid_file.display()),
            Err(e) => log!("cannot write {}: {e}", appid_file.display()),
        }
    }

    let token = if is_host {
        match gslt(&args).and_then(|t| {
            t.map(|(tok, src)| {
                let m = mask(&tok);
                std::ffi::CString::new(tok).map(|c| (c, m, src)).map_err(|_| "GSLT contains a NUL byte".to_string())
            })
            .transpose()
        }) {
            Ok(t) => t,
            Err(e) => {
                log!("FATAL: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // play: the game's own steam_api64.dll (next to the game exe); else ours next to sdr.exe.
    let game_dll_dir = game.first().and_then(|g| std::path::Path::new(g).parent().map(PathBuf::from));
    let dll_dir = match game_dll_dir {
        Some(d) if is_play && d.join("steam_api64.dll").is_file() => d,
        _ => exe_dir(),
    };
    let connect = args.iter().any(|a| a == "--connect");
    let mut play = is_play.then(|| Play {
        cmd: std::mem::take(&mut game),
        env: steam_env,
        start: Instant::now(),
        wait: connect,
        child: None,
        code: None,
    });
    let api = match load_api(&dll_dir) {
        Ok(a) => a,
        Err(e) => {
            log!("FATAL: {e}");
            die(&mut play);
        }
    };
    let mut err: ErrMsg = [0; 1024];
    let (pipe, gs, user, msgs, sockets, role_state);
    if is_host {
        let target: SocketAddr = opt(&args, "--target")
            .unwrap_or_else(|| "127.0.0.1:34197".into())
            .parse()
            .unwrap_or_else(|_| usage());
        let game_port = num("--game-port", 27016) as u16;
        let mode = num("--mode", 2) as i32;
        let mode_name = match mode {
            1 => "eServerModeNoAuthentication",
            2 => "eServerModeAuthentication",
            3 => "eServerModeAuthenticationAndSecure",
            _ => usage(),
        };
        // Interfaces this tool uses, versions as found in the shipped steam_api64.dll.
        let versions = b"SteamUtils010\0SteamNetworkingUtils004\0SteamGameServer015\0SteamNetworkingMessages002\0SteamNetworkingSockets012\0\0";
        log!(
            "SteamInternal_GameServer_Init_V2(ip=0, gamePort={game_port}, queryPort=0x{QUERY_PORT_SHARED:x} (shared), mode={mode} {mode_name}, version=\"1.0.0.0\", interfaces={:?}) ...",
            String::from_utf8_lossy(&versions[..versions.len() - 2]).replace('\0', ",")
        );
        // SAFETY: strings NUL-terminated, err writable.
        let r = unsafe {
            (api.gs_init)(0, game_port, QUERY_PORT_SHARED, mode, c"1.0.0.0".as_ptr(), versions.as_ptr().cast(), &mut err)
        };
        log!("SteamInternal_GameServer_Init_V2 = {r} ({}) errMsg='{}'", init_result_name(r), cbuf(&err));
        if r != 0 {
            std::process::exit(1);
        }
        unsafe {
            pipe = (api.gs_pipe)();
            gs = (api.gs)();
            msgs = (api.gs_msgs)();
            sockets = (api.gs_sockets)();
        }
        user = std::ptr::null_mut();
        log!("SteamGameServer_GetHSteamPipe = {pipe}; ISteamGameServer={gs:p} ISteamNetworkingMessages(gs)={msgs:p} ISteamNetworkingSockets(gs)={sockets:p}");
        role_state = Role::Host { target, idle: Duration::from_secs(num("--idle", 60)), peers: HashMap::new() };
    } else {
        let host: u64 = opt(args, "--host").and_then(|v| v.parse().ok()).unwrap_or_else(|| usage());
        let listen = if is_play {
            format!("127.0.0.1:{}", num("--port", 34200))
        } else {
            opt(args, "--listen").unwrap_or_else(|| "127.0.0.1:34200".into())
        };
        log!("host = {}", steamid_desc(host));
        log!("SteamAPI_InitFlat ...");
        // SAFETY: err writable.
        let r = unsafe { (api.api_init)(&mut err) };
        log!("SteamAPI_InitFlat = {r} ({}) errMsg='{}'", init_result_name(r), cbuf(&err));
        if r != 0 {
            die(&mut play);
        }
        unsafe {
            pipe = (api.api_pipe)();
            user = (api.user)();
            msgs = (api.msgs)();
            sockets = (api.sockets)();
        }
        gs = std::ptr::null_mut();
        log!("SteamAPI_GetHSteamPipe = {pipe}; ISteamUser={user:p} ISteamNetworkingMessages={msgs:p} ISteamNetworkingSockets={sockets:p}");
        let bound = UdpSocket::bind(&listen).or_else(|e| {
            if !is_play {
                return Err(e);
            }
            log!("bind {listen}: {e}; play falls back to a free port");
            UdpSocket::bind("127.0.0.1:0")
        });
        let sock = match bound.and_then(|s| s.set_nonblocking(true).map(|()| s)) {
            Ok(s) => s,
            Err(e) => {
                log!("FATAL: bind {listen}: {e}");
                die(&mut play);
            }
        };
        let local = sock.local_addr().map(|a| a.to_string()).unwrap_or(listen);
        log!("listening for local UDP on {local}");
        if let (Some(p), true) = (&mut play, connect) {
            p.cmd.extend(["--mp-connect".to_string(), local]);
        }
        role_state = Role::Client { host, sock, local: None, c: Counters::new() };
    }
    // steam_api.h: "must be called after SteamAPI_Init" (before it, no callback is ever delivered).
    // SAFETY: init succeeded.
    unsafe { (api.md_init)() };
    log!("SteamAPI_ManualDispatch_Init called");
    // SAFETY: init succeeded.
    let utils = unsafe { (api.utils)() };
    log!("ISteamNetworkingUtils = {utils:p}");
    if msgs.is_null() || sockets.is_null() || utils.is_null() || (is_host && gs.is_null()) || (!is_host && user.is_null()) {
        log!("FATAL: a required interface is null");
        die(&mut play);
    }
    unsafe {
        (api.set_debug)(utils, debug_level, Some(steam_debug));
        log!("SetDebugOutputFunction(level {debug_level}) done");
        if is_host {
            (api.gs_set_product)(gs, c"sdr-tunnel".as_ptr());
            log!("SetProduct(\"sdr-tunnel\") done");
            (api.gs_set_moddir)(gs, c"sdr-tunnel".as_ptr());
            log!("SetModDir(\"sdr-tunnel\") done");
            (api.gs_set_dedicated)(gs, 1);
            log!("SetDedicatedServer(true) done");
        }
        // SDK default send rate is 256 KB/s (map downloads crawl): raise it before any session exists.
        for (name, cfg, v) in [
            ("SendRateMin", CFG_SEND_RATE_MIN, rate_min),
            ("SendRateMax", CFG_SEND_RATE_MAX, rate_max),
            ("P2P_Transport_ICE_Enable", CFG_ICE_ENABLE, ice),
        ] {
            let ok = (api.set_cfg_i32)(utils, cfg, v);
            let (mut typ, mut val, mut cb) = (0i32, 0i32, std::mem::size_of::<i32>());
            let r = (api.get_cfg)(utils, cfg, CFG_SCOPE_GLOBAL, 0, &mut typ, (&mut val as *mut i32).cast(), &mut cb);
            log!("SetGlobalConfigValueInt32({name}={cfg}, {v}) = {ok}; GetConfigValue = {r}, type {typ}, value {val}");
        }
        (api.init_relay)(utils);
        log!("InitRelayNetworkAccess done");
        let a = (api.init_auth)(sockets);
        log!("InitAuthentication = {a} ({})", avail_name(a));
        match &token {
            Some((tok, m, src)) => {
                (api.gs_logon)(gs, tok.as_ptr());
                log!("LogOn({m}) called, token from {src}; result in callback 101 (ok) / 102 / 103 eResult");
            }
            None if is_host => {
                (api.gs_logon_anon)(gs);
                log!("LogOnAnonymous called (no GSLT: SteamID changes every run)");
            }
            None => {}
        }
    }
    let mut ctx = Ctx { api, pipe, is_host, gs, user, msgs, sockets, utils, ready: false, role: role_state, play, routes: Default::default() };
    ctx.run();
    let code = ctx.play.and_then(|p| p.code).unwrap_or(0);
    log!("exit {code}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steamid_decode() {
        // Anonymous game server, public universe: 0x014 type/universe nibbles.
        let id = (1u64 << 56) | (4u64 << 52) | 12345;
        assert!(steamid_valid(id));
        assert!(steamid_desc(id).contains("type=AnonGameServer universe=Public account=12345"));
        assert!(id.to_string().starts_with("9007"));
        assert!(!steamid_valid(0));
    }

    #[test]
    fn pop_and_flags() {
        assert_eq!(pop_code(((b'v' as u32) << 16) | ((b'i' as u32) << 8) | b'e' as u32), "vie");
        assert_eq!(flags_desc(16 | 1), "Unauthenticated,Relayed");
        assert_eq!(flags_desc(8), "Fast direct");
    }

    #[test]
    fn mask_hides_token() {
        assert_eq!(mask("0123456789ABCDEF"), "token len=16, ****CDEF");
        assert_eq!(mask("abc"), "token len=3, ****");
    }
}
