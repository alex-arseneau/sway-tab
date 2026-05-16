# sway-rust-alttab

Minimal Alt-Tab daemon for Sway WM.

## What it does

- Monitors window focus changes through the Sway IPC socket.
- Automatically registers `Alt+Tab` in Sway on startup.
- When you press `Alt+Tab`, Sway sends `SIGUSR1` to the daemon.
- The daemon switches focus back to the previously focused window.
- On clean shutdown (SIGTERM / SIGINT) it removes the `Alt+Tab` binding.

## Build

    cargo build --release

## Usage

Add this to your Sway config (`~/.config/sway/config`):

    exec sway-rust-alttab

Then reload Sway or log back in. Press `Alt+Tab` to switch between the current and previous window.

## Requirements

- Sway 1.10+
- `SWAYSOCK` environment variable must be set (Sway does this automatically).

## Notes

- If you already have `bindsym Alt+Tab` in your Sway config, that binding takes precedence and the daemon's binding will not execute.
- The daemon must be running for `Alt+Tab` to work. If it crashes or is killed with `SIGKILL`, the binding may remain in Sway as a no-op until you restart the daemon or manually run `unbindsym Alt+Tab`.
