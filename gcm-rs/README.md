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
- **Tunnel manager**: the toolbar's *⇄ Tunnels* button opens a list of saved port forwards (local `-L`,
  remote `-R`, SOCKS `-D`). Each runs through a saved SSH session on its own, without a terminal tab, and can
  be started, stopped and set to start with gcm. A coloured dot shows its state: grey stopped, yellow
  connecting, green up, red failed with ssh's error underneath. See *Tunnels* below.
- **Per-session colours**: give a session its own text and background colour (*Colours* in the session
  editor), e.g. a red background for production. Imported from GCM's `font-color`/`back-color`.
- **Session logging**: tick *Save terminal output to a log file* on a session, or use *Start logging* /
  *Stop logging* in any tab's right-click menu. A ● in the tab title shows it's on. See *Logs* below.
- **MultiExec**: the toolbar's *⚡ MultiExec* button sends whatever you type in one terminal to every
  terminal in the group, like MobaXterm's MultiExec or GCM's cluster mode. All tabs start in the group;
  right-click a tab and untick *Include in MultiExec* to leave one out. Grouped tabs show ⚡ and an
  orange border. The bar under the toolbar also has *All*/*None* buttons and a command line that sends
  one line to the whole group. Keys are re-encoded for each target, so arrow keys work even when one
  terminal is in an application like vim or less.
- **Quick connect** in the toolbar: `user@host:2222`, `telnet://router`, `rdp://winbox`,
  `serial://COM3:9600`.
- **Password autofill**: replaces `ssh.expect`. A saved password is typed when the server first prints
  `password:`, and autofill switches off as soon as you type anything yourself.
- **Startup commands** per session, using GCM's `##D=<ms>` delay syntax.
- **Import** from GNOME Connection Manager, MobaXterm and PuTTY (*Session → Import sessions*). See *Importing*
  below.
- **Terminal**: alacritty's VT engine with 256 colours and truecolour, mouse reporting (mc, htop, vim),
  bracketed paste, scrollback, and fallback fonts for CJK and symbols.

## Importing

*Session → Import sessions* (or *Import…* on the start screen) shows what it found before adding anything.
Sessions already in your list (same name, folder, host, port and user) are left alone, so importing again is
safe. Anything that can't be imported is listed with the reason.

| Source | What is read |
|---|---|
| **GNOME Connection Manager** | `~/.gcm/gcm.conf`: hosts, folders, saved passwords, tunnels, startup commands, colours, log flag. |
| **MobaXterm** | A `.mxtsessions` export (*Sessions → Export*): SSH, SFTP, RDP and VNC sessions with their folders, user, port, private key, jump hosts, X11/compression, login command, comment, colours and log flag. |
| **PuTTY** (this computer) | On Windows the registry (`HKCU\Software\SimonTatham\PuTTY\Sessions`, read with `reg export`); elsewhere `~/.putty/sessions`. SSH, Telnet and serial sessions with user, port, key, X11/agent forwarding, compression, keep-alive, port forwards, colours and log flag. They go in a *PuTTY* folder. |
| **PuTTY `.reg` file** | The same, from a registry export made on another machine: `reg export HKCU\Software\SimonTatham\PuTTY\Sessions putty.reg`. KiTTY exports work too. |

Notes:

- MobaXterm's session format isn't documented. The importer follows the community write-up of
  [the `.mxtsessions` format](https://gist.github.com/Ruzgfpegk/ab597838e4abbe8de30d7224afd062ea). Telnet,
  serial and local-shell entries aren't covered there, so they are listed as not imported rather than
  guessed at.
- Neither MobaXterm nor PuTTY exports passwords. Saved PuTTY keys are `.ppk` files, which OpenSSH can't
  read: convert them with `puttygen key.ppk -O private-openssh -o key` and point the session at the result.
- PuTTY's rlogin, raw and SUPDUP sessions have no equivalent here and are skipped.

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

A saved password is handed to ssh through `SSH_ASKPASS`, which points back at `gcm-rs` itself. That helper
answers only password and passphrase prompts. It refuses anything else, including new host keys and
one-time codes, so if a host is new, connect once in an SSH tab to accept its key, then press *Reconnect*
in the SFTP pane. The password reaches the helper through the `sftp` process's environment, which other
processes running as you can read.

Folders can be downloaded and uploaded recursively. *Delete* removes files and **empty** folders only,
because sftp has no recursive delete.

## Tunnels

Each tunnel runs `ssh -N -o ExitOnForwardFailure=yes` with the chosen session's host, port, user, key and
`-o`/`-J`/`-F` options. That means a forward that can't be set up makes ssh exit with its own error message
instead of hanging. Without a keep-alive in the session, `ServerAliveInterval=30` is added so a dead link
shows up as failed. Passwords use the same askpass helper as SFTP.

- A local or SOCKS tunnel counts as **up** once its port is listening. gcm checks whether the port is
  free before starting, so a clash fails straight away. A remote tunnel counts as up once ssh has stayed
  connected for three seconds.
- Tunnels stop when gcm exits. That also holds if gcm is killed or crashes: on Linux the kernel signals
  them (`PR_SET_PDEATHSIG`), and on Windows they run in a job object that closes with gcm.
- The *Tunnels* field on an SSH session still works too. Those forwards live and die with that terminal tab.

## Logs

Each session start writes `<session>_<YYYYmmdd_HHMMSS>.log` to the log folder
(`~/.config/gcm-rs/logs`, `%APPDATA%\gcm-rs\logs`, or *Log folder* in Settings). *Session → Open log
folder* opens it.

- The file is plain text with colours and cursor movement removed. A progress counter that redraws a
  line keeps only its final state, and backspaced characters disappear.
- Each file starts with a header line and, when logging stops normally, ends with a closing line.
- The log holds whatever was on screen, so on Linux and macOS it is readable only by you (mode 0600).
  Passwords typed at a prompt aren't echoed, so they don't appear in it.

## Command line

```
gcm-rs [SESSION | [proto://][user@]host[:port]]...
```

Each argument opens a tab: either the name of a saved session or a quick-connect address.
`man gcm-rs` has the details once the package is installed.

## Packages

Debian/Ubuntu and Fedora/RHEL/openSUSE packages install the `gcm-rs` command, a menu entry, an icon and a
man page. They can be installed next to the original Python GCM.

The packages are built against glibc 2.28, so they install on Debian 10+, Ubuntu 20.04+,
RHEL/Rocky/Alma 8+, Fedora and openSUSE Leap 15+ (x86_64).

```sh
sudo apt install ./gcm-rs_0.1.0-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./gcm-rs-0.1.0-1.x86_64.rpm    # Fedora, RHEL
sudo zypper install ./gcm-rs-0.1.0-1.x86_64.rpm # openSUSE
```

`apt`/`dnf` also pull in what gcm-rs loads at run time (xkbcommon, OpenGL/EGL) and the recommended
OpenSSH client. Telnet, FreeRDP, a VNC viewer and CJK fonts are optional extras (*Suggests*).

Where to get them:

- **From CI**: every push builds both packages. They're in the *gcm-rs-packages* artifact of the workflow
  run.
- **Releases**: pushing a tag `gcm-rs-v<version>` that matches `Cargo.toml` publishes a GitHub release
  with the `.deb`, the `.rpm` and a Windows `.zip`.
- **Build them yourself**:

  ```sh
  cargo install --locked cargo-deb cargo-generate-rpm cargo-zigbuild
  pip install ziglang         # or install zig from your distribution
  cd gcm-rs
  # Link against glibc 2.28 so the package runs on older distributions too.
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
  cargo deb --no-build --target x86_64-unknown-linux-gnu
  cargo generate-rpm --target x86_64-unknown-linux-gnu --payload-compress gzip
  # -> target/x86_64-unknown-linux-gnu/{debian,generate-rpm}/
  ```

  A plain `cargo build --release` also works, but the result needs the glibc of the machine it was built on.
  `--payload-compress gzip` keeps the RPM readable by older rpm versions.

Package metadata (dependencies, files) lives in `Cargo.toml` under `[package.metadata.deb]` and
`[package.metadata.generate-rpm]`.

## Building

You need Rust 1.95 or newer ([rustup.rs](https://rustup.rs)).

```sh
cd gcm-rs
cargo build --release
./target/release/gcm-rs       # Windows: target\release\gcm-rs.exe
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

- IME input inside terminal tabs

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Startup and command-line handling |
| `src/app.rs` | Window, session tree, tabs, dialogs |
| `src/terminal.rs` | Terminal widget: PTY/serial I/O, rendering, keyboard and mouse |
| `src/sftp.rs` | SFTP browser: drives the system `sftp`, file table |
| `src/tunnel.rs` | Tunnel manager: saved forwards, `ssh -N` processes, status |
| `src/sessionlog.rs` | Session logs: escape-code stripping, log files |
| `src/launch.rs` | Session → command line (ssh/telnet args, RDP/VNC templates), askpass helper |
| `src/config.rs` | Sessions, settings, TOML storage, password encryption |
| `src/gcm_import.rs` | Reader for the original GCM config and its pyAES passwords |
| `src/mobaxterm.rs` | Reader for MobaXterm `.mxtsessions` exports |
| `src/putty.rs` | Reader for PuTTY sessions (registry export, `.reg` files, `~/.putty/sessions`) |
| `src/fonts.rs` | System fallback fonts |

`cargo test` covers argument building, quick-connect parsing, key encoding, autofill, config round-trips
and GCM import (checked against output from the original `pyAES.py`). Where OpenSSH's `sftp-server` is
installed it also runs a full SFTP session (list, cd, upload, download, mkdir, rename, delete, errors)
against a local server, with no network involved.
