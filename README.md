# sdr

UDP <-> Steam Datagram Relay tunnel, so friends reach a UDP game server (Factorio) on a box behind CGNAT. Logs every Steam step.

## Players: just click Play

Steam -> Factorio -> Properties -> Launch options (replace `<HOST_ID>` with the SteamID64 the host gives you):

    cmd /c "curl -sfLRz sdr.exe --remove-on-error --connect-timeout 5 -o sdr.exe https://github.com/UberMorgott/sdr/releases/latest/download/sdr.exe & .\sdr.exe play --host <HOST_ID> -- %command%"

What it does:
- `curl -z` downloads `sdr.exe` into the Factorio folder only when the release is newer (HTTP 304 otherwise); offline it keeps the old one.
- `sdr play` starts the tunnel on `127.0.0.1:34200`, starts Factorio as usual, and exits when Factorio exits.
- In Factorio: Multiplayer -> Connect to address -> `127.0.0.1:34200`. After a disconnect, connect to the same address again.
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
`<id>` to your friends. Each Steam peer gets its own local UDP socket forwarding to `--target`.
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

## Client (manual, Steam client running and logged in)

    sdr client [--appid 427520] --host <SteamID64 from host> [--listen 127.0.0.1:34200]

Then in Factorio: Multiplayer -> Connect to address -> `127.0.0.1:34200`.

## Network tuning (all roles)

- `--send-rate-min 1000000 --send-rate-max 10000000` (bytes/s): Steam's default is 256 KB/s, which makes map downloads crawl.
  Steam currently sends at the min value, so raise `--send-rate-min` on both ends for faster downloads.
- `--ice all|private|public|off` (default `all`): allow direct P2P (ICE) besides relays. The log shows `ROUTE ... RELAYED|DIRECT`.

## Logs

stdout and `sdr-<role>.log` next to the exe (`host`/`client`: appended; `play`: `sdr.log`, overwritten), flushed per line:
every init call and result, every Steam callback by id, Steam networking debug output (`steamnet[...]`), a diag block
every 2 s until READY, every 30 s after (logon, SteamID, public IP, relay/auth status, ping location), per-session
state and realtime stats (`RT`: ping, quality, in/out rate, send rate, queue) every 5 s, and the 10 best relay POPs
at READY. Ctrl-C logs off and shuts Steam down.
