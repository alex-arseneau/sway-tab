# sway-tab

History-aware Alt-Tab daemon for Sway WM.

## What it does

- Monitors window focus changes through the Sway IPC socket.
- Automatically registers `Alt+Tab` in Sway on startup.
- Maintains a history of recently focused windows (up to 15).
- On first `Alt+Tab`, switches to the last active window.
- While holding Alt and pressing Tab repeatedly, cycles through the full window history.
- On Alt key release, the currently previewed window is promoted to most-recent.
- History wraps around circularly — keep pressing Tab and you'll eventually return to where you started.
- On clean shutdown (SIGTERM / SIGINT) it removes the `Alt+Tab` binding.

## Build

    cargo build --release

## Usage

Add this to your Sway config (`~/.config/sway/config`):

    exec sway-tab

Then reload Sway or log back in. Press `Alt+Tab` to cycle through recent windows; press Tab repeatedly while holding Alt to go further back in history.

## Requirements

- Sway 1.10+
- `SWAYSOCK` environment variable must be set (Sway does this automatically).

## Notes

- If you already have `bindsym Alt+Tab` in your Sway config, that binding takes precedence and the daemon's binding will not execute.
- The daemon must be running for `Alt+Tab` to work. If it crashes or is killed with `SIGKILL`, the binding may remain in Sway as a no-op until you restart the daemon or manually run `unbindsym Alt+Tab`.
