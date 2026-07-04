# sway-tab

History-aware Alt-Tab daemon for Sway WM.

## What it does

- Monitors window focus changes through the Sway IPC socket.
- Automatically registers `Alt+Tab` in Sway on startup.
- Maintains a history of recently focused windows.
- On first `Alt+Tab`, switches to the last active window.
- While holding Alt and pressing Tab repeatedly, cycles through the full window history.
- The previewed window is committed (promoted to most-recent) once you settle on it: either after a short timeout with no further `Alt+Tab` activity (see `--timeout`), or as soon as you switch focus another way (clicking a window or using a different shortcut).
- History wraps around circularly — keep pressing Tab and you'll eventually return to where you started.
- On clean shutdown (SIGTERM / SIGINT) it removes the `Alt+Tab` binding.

## Build

    cargo build --release

## Usage

Add this to your Sway config (`~/.config/sway/config`):

    exec sway-tab

Then reload Sway or log back in. Press `Alt+Tab` to cycle through recent windows; press Tab repeatedly while holding Alt to go further back in history.

## Options

- `--timeout N`, `-t N` — seconds of inactivity before the previewed window is committed (default: 4.0).
- `--trace` — enable verbose trace logging to stderr.
- `--help`, `-h` — print usage and exit.

For example, to shorten the commit timeout to 2 seconds:

    exec sway-tab -t 2

## Requirements

- Sway 1.10+
- `SWAYSOCK` environment variable must be set (Sway does this automatically).

## Notes

- If you already have `bindsym Alt+Tab` in your Sway config, that binding takes precedence and the daemon's binding will not execute.
- The daemon must be running for `Alt+Tab` to work. If it crashes or is killed with `SIGKILL`, the binding may remain in Sway as a no-op until you restart the daemon or manually run `unbindsym Alt+Tab`. The daemon registers a second binding too, so you may also need `unbindsym --release Alt+Tab` after an unclean shutdown.

## Inspired By

- [sway-alttab](https://github.com/autolyticus/sway-alttab)
