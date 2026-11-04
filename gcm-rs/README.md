# gcm-rs: GNOME Connection Manager in Rust

A rewrite of GNOME Connection Manager in Rust with [egui](https://github.com/emilk/egui), aimed at a
MobaXterm-style workflow on both **Linux and Windows**. It builds to a single binary with no Python, GTK
or `expect` needed.

It lives next to the original Python app while it matures.

## Features

- **Session tree**: saved sessions in nested folders (`prod/web`), with a filter box, double-click to
  connect, and right-click to edit, duplicate or delete.
- **Tabs and splits**: drag a tab to any edge to split the view, or right-click a tab for
  *Split right* / *Split down* / *Duplicate* / *Reconnect*.
- **Protocols**:
  - **SSH**: runs the system `ssh`, so `~/.ssh/config`, keys, the agent and `known_hosts` keep working.
    Supports tunnels (`L`/`R`/`D`), X11 and agent forwarding, compression, keep-alive and extra arguments.
  - **Telnet**: runs the system `telnet`, typing the saved user name and password at the prompts.
  - **Serial**: opens the port directly (`/dev/ttyUSB0`, `COM3`), with speed, data bits, parity, stop
    bits and flow control. The device field lists the ports it finds.
  - **RDP / VNC**: starts your normal client from a command template (see *Settings*).
  - **Local shell**: `$SHELL` on Linux, PowerShell on Windows, or any program you name.
- **SFTP side browser**: the sidebar's *SFTP* tab shows the files of the SSH session in the focused tab.
  Double-click to open a folder or download a file, Ctrl+click to select several, drop files from your
  desktop onto the window to upload them, and right-click for rename, delete or copy path. New folders and
  downloads go to your Downloads folder (configurable). It runs the system `sftp` client, so authentication
  works the same as in the SSH tab (see *SFTP* below).
- **Quick connect** in the toolbar: `user@host:2222`, `telnet://router`, `rdp://winbox`,
  `serial://COM3:9600`.
- **Password autofill**: replaces `ssh.expect`. A saved password is typed when the server first prints
  `password:`, and autofill switches off as soon as you type anything yourself.
- **Startup commands** per session, using GCM's `##D=<ms>` delay syntax.
- **Import from GCM**: *Session → Import from GNOME Connection Manager* reads `~/.gcm/gcm.conf`, including
  its encrypted passwords.
- **Terminal**: alacritty's VT engine with 256 colours and truecolour, mouse reporting (mc, htop, vim),
  bracketed paste, scrollback, and fallback fonts for CJK and symbols.

## Keys and mouse

| Action | How |
|---|---|
| Copy / paste | `Ctrl+Shift+C` / `Ctrl+Shift+V`, `Ctrl+Insert` / `Shift+Insert` |
| Paste | Right-click (or turn off *Paste on right click* in Settings to get a menu instead) |
| Select word / line / block | Double-click / triple-click / `Alt`+drag |
| Scrollback | Mouse wheel, `Shift+PgUp` / `Shift+PgDn`, `Shift+Home` / `Shift+End` |
| Select text in mouse apps | Hold `Shift` while dragging |
| Stopped session | `R` reconnects, `Enter` closes the tab |

On Windows `Ctrl+C` copies when text is selected and sends an interrupt otherwise, as Windows Terminal
does. On Linux it always sends an interrupt.

## SFTP

The browser opens a second connection with your system `sftp` (it ships with OpenSSH on Linux and on
Windows 10/11), using the session's host, port, user, key, keep-alive, compression and any `-o`/`-J`/`-F`
options from *Extra arguments*. Keys, the agent, `~/.ssh/config` and `known_hosts` apply as usual.

A saved password is handed to ssh through `SSH_ASKPASS`, which points back at `gcm` itself. That helper
answers only password and passphrase prompts. It refuses anything else, including new host keys and
one-time codes, so if a host is new, connect once in an SSH tab to accept its key, then press *Reconnect*
in the SFTP pane. The password reaches the helper through the `sftp` process's environment, which other
processes running as you can read.

Folders can be downloaded and uploaded recursively. *Delete* removes files and **empty** folders only,
because sftp has no recursive delete.

## Command line

```
gcm [SESSION | [proto://][user@]host[:port]]...
```

Each argument opens a tab: either the name of a saved session or a quick-connect address.

## Building

You need Rust 1.95 or newer ([rustup.rs](https://rustup.rs)).

```sh
cd gcm-rs
cargo build --release
./target/release/gcm          # Windows: target\release\gcm.exe
```

### Linux

There are no build dependencies beyond a C toolchain. At runtime it needs what any desktop already has:
OpenGL, `libxkbcommon-x11` (X11) or a Wayland compositor, and `ssh`/`telnet` for those protocols.
Fallback fonts are picked up from DejaVu and Noto CJK if installed.

### Windows

- **SSH**: Windows 10 1809+ and Windows 11 ship the OpenSSH client (*Settings → Optional features →
  OpenSSH Client*).
- **Telnet**: enable *Telnet Client* under *Turn Windows features on or off*.
- **RDP** uses `mstsc` and **VNC** expects `vncviewer.exe` (TigerVNC, RealVNC, …) on `PATH`. Change
  either in Settings.

## Settings and files

| | Linux | Windows |
|---|---|---|
| Config | `~/.config/gcm-rs/gcm.toml` | `%APPDATA%\gcm-rs\gcm.toml` |
| Password key | `~/.config/gcm-rs/key` (mode 0600) | `%APPDATA%\gcm-rs\key` |

`gcm.toml` is plain TOML and can be edited by hand. Passwords are stored AES-256-GCM encrypted with the
random key in `key`. As with the original GCM, this only keeps them out of plain sight: anyone who can
read both files can recover them. **Prefer SSH keys.**

The RDP and VNC commands are templates that accept `{host}`, `{port}`, `{user}` and `{password}`. If a
placeholder's value is empty, the whole argument containing it is dropped, so `/p:{password}` vanishes
when no password is saved. Defaults:

- Linux: `xfreerdp /v:{host}:{port} /u:{user} /p:{password} /cert:tofu /dynamic-resolution` and
  `vncviewer {host}::{port}`
- Windows: `mstsc /v:{host}:{port}` and `vncviewer.exe {host}::{port}`

## Not yet

- Multi-exec (typing into several tabs at once, GCM's "cluster" mode)
- Tunnel manager UI (tunnels currently live on each SSH session)
- IME input inside terminal tabs
- Per-session colours and session logging

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Startup and command-line handling |
| `src/app.rs` | Window, session tree, tabs, dialogs |
| `src/terminal.rs` | Terminal widget: PTY/serial I/O, rendering, keyboard and mouse |
| `src/sftp.rs` | SFTP browser: drives the system `sftp`, askpass helper, file table |
| `src/launch.rs` | Session → command line (ssh/telnet args, RDP/VNC templates) |
| `src/config.rs` | Sessions, settings, TOML storage, password encryption |
| `src/gcm_import.rs` | Reader for the original GCM config and its pyAES passwords |
| `src/fonts.rs` | System fallback fonts |

`cargo test` covers argument building, quick-connect parsing, key encoding, autofill, config round-trips
and GCM import (checked against output from the original `pyAES.py`). Where OpenSSH's `sftp-server` is
installed it also runs a full SFTP session (list, cd, upload, download, mkdir, rename, delete, errors)
against a local server, with no network involved.
