# sdr

UDP <-> Steam Datagram Relay tunnel, so friends reach a UDP game server (Factorio) on a box behind CGNAT. Logs every Steam step.

## Players: just click Play

Steam -> Factorio -> Properties -> Launch options (replace `<HOST_STEAMID>` with the host's SteamID64; the ready-to-paste line for our server is shared privately):

    cmd /c "curl -sfLRz sdr.exe --remove-on-error --connect-timeout 5 -o sdr.exe https://github.com/UberMorgott/sdr/releases/latest/download/sdr.exe & .\sdr.exe play --host <HOST_STEAMID> -- %command%"

The host prints its SteamID64 on start (`READY steamid=<id>`).
Start Factorio with Steam's Play button: started outside Steam, Factorio quits at once while `sdr` holds the Steam API.

What it does:
- `curl -z` downloads `sdr.exe` into the Factorio folder only when the release is newer (HTTP 304 otherwise); offline it keeps the old one.
- `sdr play` starts the tunnel on `127.0.0.1:34200`, starts Factorio as usual, and exits when Factorio exits, with
  Factorio's exit code. Factorio restarts itself after mod sync / settings changes: `sdr` then follows any process with
  the same exe path and exits 5 s after the last one is gone.
- In Factorio: Multiplayer -> Connect to address -> `127.0.0.1:34200`. After a disconnect, connect to the same address again.
  If the server has a game password, Factorio asks for it (`--connect` cannot pass one: the join is refused with `PasswordMissing`).
  If port 34200 is busy, a free port is used: see `sdr.log`.
- Files added to the Factorio folder: `sdr.exe` and `sdr.log` (overwritten each run). It uses Factorio's own
  `steam_api64.dll` and never writes `steam_appid.txt`.
- The console window from `cmd` stays open while you play; closing it stops the tunnel.

`sdr play` options: `--host <SteamID64>` (required), `--port 34200`, `--appid 427520`, `--connect` (also pass
`--mp-connect 127.0.0.1:<port>` to Factorio, so it joins right away once the tunnel is READY or after 20 s).

## Build

    cargo build --release

`build.rs` copies `steam_api64.dll`, `steamclient64.dll`, `tier0_s64.dll`, `vstdlib_s64.dll` next to the exe
(from `STEAM_DLL_DIR`, default `D:\Steam\steamapps\common\porthole`). `host` and `client` need those DLLs next to
`sdr.exe`; `play` needs none (it loads the game's `steam_api64.dll`).

## Host (server box, no Steam client needed)

    sdr host [--appid 427520] [--target 127.0.0.1:34197] [--game-port 27016] [--mode 2] [--idle 60] [--debug-level 5]

Logs on as a Steam game server. Wait for the `READY steamid=<id> relay=Current` line and give
`<id>` to your friends. Each Steam peer gets its own local UDP socket forwarding to `--target` (IPv4 only);
a peer idle in both directions for `--idle` seconds is dropped. `client`/`play` accept Steam sessions from `--host` only.
`--mode`: 1 NoAuthentication, 2 Authentication, 3 AuthenticationAndSecure.
Host and players must use the same `--appid` (427520 = Factorio by default; 480 also works).

### Persistent SteamID (GSLT)

Anonymous logon gets a new SteamID every run. For a fixed one, use a Game Server Login Token:

1. Create a token at https://steamcommunity.com/dev/managegameservers with App ID = the `--appid` you run with (427520 by default).
2. Put it in `gslt-<appid>.txt` (e.g. `gslt-427520.txt`) or `gslt.txt` next to the exe (or pass `--gslt-file <path>`, or set env `SDR_GSLT`).
   Lookup order: `--gslt-file`, `SDR_GSLT`, `gslt-<appid>.txt`, `gslt.txt`.
3. The log shows `LogOn(token len=N, ****last4) called`; `READY ... type=GameServer` (not `AnonGameServer`) means it worked.
   A bad/expired token shows up as callback 102/103 with its eResult (e.g. `InvalidParam`, `AccessDenied`, `GSLTDenied`, `Expired`).

One token per running server. The token is tied to the Steam account that created it: keep it secret (`gslt*.txt` is git-ignored).

### Autostart (Windows scheduled task)

`run-host.ps1` next to `sdr.exe` restarts the host 5 s after any exit or crash:

    Set-Location $PSScriptRoot
    while ($true) { & "$PSScriptRoot\sdr.exe" host --target 127.0.0.1:34197; Start-Sleep 5 }

Task `sdr-host`: at startup, as SYSTEM, no time limit, ignore new instances, restart on failure every 1 min:

    $d = 'D:\SERVERS\sdr-tunnel'
    $a = New-ScheduledTaskAction -Execute (Get-Command pwsh).Source -WorkingDirectory $d -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$d\run-host.ps1`""
    $s = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit 0 -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -StartWhenAvailable
    Register-ScheduledTask sdr-host -Action $a -Trigger (New-ScheduledTaskTrigger -AtStartup) -Settings $s -Principal (New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest)
    Start-ScheduledTask sdr-host

Restart / update: `Stop-ScheduledTask sdr-host` (stops the loop and `sdr.exe`), swap `sdr.exe`, `Start-ScheduledTask sdr-host`.
Undo: `Stop-ScheduledTask sdr-host; Unregister-ScheduledTask sdr-host -Confirm:$false`.

## Client (manual, Steam client running and logged in)

    sdr client [--appid 427520] --host <SteamID64 from host> [--listen 127.0.0.1:34200]

Then in Factorio: Multiplayer -> Connect to address -> `127.0.0.1:34200`.

## Network tuning (all roles)

- `--send-rate 1000000` (bytes/s): sets Steam's SendRateMin = SendRateMax (Steam's default is 256 KB/s, which makes
  map downloads crawl). Steam does no bandwidth probing: it sends at the min value, so one fixed rate is set
  (`--send-rate-min` / `--send-rate-max` still override one side). Keep it at or just below what the path
  carries: above it the excess is lost and goodput collapses. Measured 2026-11-11 Georgia <- St. Petersburg
  (relay vie/sto, ping 94 ms), 12 s UDP blasts: min 0.5 MB/s -> 0.48 MB/s clean; 1 MB/s -> 0.75-0.96 MB/s;
  1.3 MB/s -> 1.2 MB/s once; 2 MB/s -> 0.7 MB/s (quality 0.65); 4 MB/s -> 0.28 MB/s (quality 0.3).
  Re-measured with v0.3.0 (4 MiB socket/Steam buffers), 10 s blasts, 3+ runs each, loss per run:
  1 MB/s 0-3.1 % (quality 0.96-1.0); 1.2 MB/s 0, 0, 6.8 %; 1.3 MB/s 0-15 %; 1.5 MB/s 46-51 % (quality 0.7);
  2 MB/s 70-76 % (quality 0.5). The path itself caps near 1.2 MB/s and has bad minutes (~25 % loss at any rate),
  so the bigger buffers do not move the cliff: the default 1 MB/s stays.
  v0.4.0 checks (2026-11-11, 1200 B, 5 s warmup + 20 s, 3 rounds): a real map download (5.0 MB, 469 KB/s) left the
  host's Steam queue empty at 0.5-0.57 MB/s out, so Factorio's own block transfer, not the tunnel, set the pace.
  Two host identities in parallel at 0.9 MB/s each gave 1.47 / 1.48 / 1.75 MB/s together (2-19 % loss) vs one at
  1.8 MB/s: 0.95-1.15 MB/s (36-47 % loss): not a reason for multipath while Factorio is the limit.
  Forcing the host's relay cluster (`SDRClient_ForceRelayCluster`) to hel/waw/fra was never better than auto (sto, 96 ms):
  waw +30 ms, fra +18 ms, same loss; so there is no option for it.
- `--send-buffer <bytes>`: Steam's per-session SendBufferSize (Steam default 512 KB; ours: send rate x 150 ms,
  min 64 KiB, i.e. 150 KB at 1 MB/s). Queued data is latency: v0.3.0's 4 MiB allowed ~4 s at 1 MB/s. When it is full,
  SendMessage returns `LimitExceeded`; such datagrams wait in a small per-peer retry queue (max 50 ms / 1 MB),
  then are dropped. The `SESSION` log line counts `LimitExceeded`, the retry queue and drops; `RT` shows
  Steam's own queue (`pendingUnrel`, `queue` in us).
- `--dual-wifi`: allow Steam's dual-band Wi-Fi multipath (`DualWifi_Enable`). Off by default: on a Qualcomm
  dual-band card the second socket failed every send (`WSAENETUNREACH`, 27 retries per session).
- `--dump <path>` (or env `SDR_DUMP`): append every forwarded datagram to a binary file (overwritten per run, stops
  at 200 MB). Record, little-endian: `[u64 unix micros][u8 dir: 0 game->tunnel, 1 tunnel->game][u32 len][bytes]`.
  Host role: all peers in one file, no peer id. Off by default.
- Small datagrams are coalesced (Steam Nagle, `NagleTime` 2 ms; v0.4.0 and older sent each with `NoNagle`). In a real
  map download Factorio sent ~2150 tiny datagrams/s upstream = ~1135 wire packets/s and the host got only ~45 % of
  them (quality 0.55). Bench client -> host, 13 B: 2200/s lost 18 %, 4000/s 52 % (path passes ~1000 pkt/s); with
  Nagle 2 ms: 0 % at 2200/s and 4000/s (~300 pkt/s on the wire), also with a parallel 0.6-1 MB/s download;
  RTT p50 102 -> 105 ms (5 ms default: 111 ms).
- Local UDP sockets get 4 MiB `SO_RCVBUF`/`SO_SNDBUF` (Windows default 64 KB); the read-back is logged.
- `--ice all|private|public|off` (default `all`): allow direct P2P (ICE) besides relays. The log shows `ROUTE ... RELAYED|DIRECT`.

## Logs

stdout and `sdr-<role>.log` next to the exe (`host`/`client`: appended; `play`: `sdr.log`, overwritten), written by a
logger thread and flushed per line (a full queue drops lines and logs how many):
every init call and result, every Steam callback by id, Steam networking debug output (`steamnet[...]`), a diag block
every 2 s until READY, every 30 s after (logon, SteamID, public IP, relay/auth status, ping location), per-session
state and realtime stats (`RT`: ping, quality, in/out rate, send rate, queue) every 5 s, and the 10 best relay POPs
at READY. Steam's text is decoded from the Windows ANSI code page (no mojibake on Russian Windows).
Ctrl-C or closing the console closes the sessions; the host logs off and shuts the game server down. `client`/`play`
skip `SteamAPI_Shutdown`: it crashed (0xC0000005) in 2 of 8 local runs and the game's exit code was lost; process
exit releases Steam anyway. A 5 s watchdog exits with the right code if Steam teardown hangs.

The host drops a peer on session failure (callback 1252), on `ClosedByPeer` / `ProblemDetectedLocally`, or after
`--idle` seconds without traffic Steam accepted.
