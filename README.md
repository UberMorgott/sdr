# sdr-tunnel

UDP <-> Steam Datagram Relay tunnel, so friends reach a UDP game server (Factorio) on a box behind CGNAT. Logs every Steam step.

## Build

    cargo build --release

`build.rs` copies `steam_api64.dll`, `steamclient64.dll`, `tier0_s64.dll`, `vstdlib_s64.dll` next to the exe
(from `STEAM_DLL_DIR`, default `D:\Steam\steamapps\common\porthole`). Ship the whole `target\release` folder
(exe + those 4 DLLs).

## Host (server box, no Steam client needed)

    sdr-tunnel host --appid 480 --target 127.0.0.1:34197 [--game-port 27016] [--mode 2] [--idle 60] [--debug-level 5]

Logs on as an anonymous Steam game server. Wait for the `READY steamid=<id> relay=Current` line and give
`<id>` to your friends. Each Steam peer gets its own local UDP socket forwarding to `--target`.
`--mode`: 1 NoAuthentication, 2 Authentication, 3 AuthenticationAndSecure.

## Client (friend's PC, Steam client running and logged in)

    sdr-tunnel client --appid 480 --host <SteamID64 from host> [--listen 127.0.0.1:34200]

Then in Factorio: Multiplayer -> Connect to address -> `127.0.0.1:34200`.

## Logs

stdout and `sdr-tunnel-<role>.log` next to the exe (appended, flushed per line): every init call and result,
every Steam callback by id, Steam networking debug output (`steamnet[...]`), and a diag block every 2 s until
READY, every 30 s after (logon, SteamID, public IP, relay/auth status, ping location, per-session state).
Ctrl-C logs off and shuts Steam down.
