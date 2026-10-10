# Flatshot (Rust)

A ShareX-style screenshot tool for Linux on Wayland, built mainly for KDE Plasma: a rewrite of [Flatshot](https://github.com/nicholas-sbx/screenshot) as a single native binary, with no Python, Qt or helper programs.

Flatshot lives in your system tray. Press your shortcut and the screen freezes. **Drag a region, or click a window, and it's saved and copied straight away.** To mark it up first, draw on the frozen screen with the floating toolbar, then drag. **QR codes and barcodes** on screen are found automatically, each with a card to copy or open it.

## Why the rewrite

- **Faster to open.** The tray app keeps its connection to the compositor and everything it needs loaded. A shortcut press goes straight from KDE's shortcut service to a capture: KWin's screenshot API writes the raw pixels into Flatshot's memory, the overlay's surfaces are set up while that happens, and the first frame is painted as soon as the pixels are in. No process starts, nothing is encoded or decoded on the way. Run `flatshot --tray -v` to see each step's time.
- **Less memory.** No interpreter or GUI toolkit: an idle tray app is a few megabytes, and every capture gives its memory back when it closes. Pictures are shared with the compositor, not copied; drawing repaints only what changed.
- **Safer.** Memory-safe code throughout, including the QR and barcode decoders, which read whatever is on screen. Files are written without following links or replacing anything that exists; the IPC socket, scripts and temporary files are private to you; text from the screen (code contents, window titles) is shown as plain text, never as markup; only KDE's own services are listened to for shortcuts and notification clicks. See [Security](#security).

### Measured

Both versions on the same machine and desktop (a headless Sway, 1600 × 1000, software rendering), from the moment of the key press, or of starting the program, to the overlay's first frame on screen (taken from the Wayland protocol trace), and the tray app's memory:

| | Flatshot (Python) | Flatshot (Rust) |
|---|---|---|
| Shortcut → overlay on screen, tray app running | 100 ms (87–124) | **19 ms** (18–20) |
| `flatshot` → overlay on screen, no tray app | 324 ms (303–350) | **24 ms** (22–30) |
| Tray app, idle | 65 MB | **12 MB** |
| Tray app, after six captures | 93 MB | **16 MB** |
| Peak during a capture | 124 MB | **40 MB** |

Most of the Rust version's 19 ms is the compositor copying the screen. (Memory is resident set size; times are means of 6–7 runs, ranges in brackets.)

## Install

Each release has packages for Debian/Ubuntu (`.deb`), Fedora/openSUSE (`.rpm`) and Arch (`.pkg.tar.zst`):

| Distro | Install |
|---|---|
| Debian 12+, Ubuntu 22.04+, KDE neon | `sudo apt install ./flatshot_*_amd64.deb` |
| Fedora / openSUSE | `sudo dnf install ./flatshot-*.x86_64.rpm` |
| Arch / Manjaro / EndeavourOS | `sudo pacman -U flatshot-*-x86_64.pkg.tar.zst`, or the pacman repository below |

### Arch: pacman repository

Each release also updates a pacman repository, so Flatshot updates with the rest of your system:

```sh
sudo tee -a /etc/pacman.conf <<'EOF'

[flatshot]
SigLevel = Optional TrustAll
Server = https://github.com/nicholas-sbx/screenshot-rs/releases/download/pacman
EOF
sudo pacman -Sy flatshot
```

`sudo pacman -Syu` then picks up new versions. To remove it, delete the `[flatshot]` block and `sudo pacman -R flatshot`.

- **The repository has to be public** for pacman to download from it: GitHub only serves a private repository's releases to people signed in.
- The packages are signed when the release workflow has a `PACMAN_GPG_KEY` secret; until then `SigLevel = Optional TrustAll` accepts them unsigned.

### From source

Rust 1.88 or newer and libxkbcommon (with its headers):

```sh
cargo build --release
sudo install -Dm755 target/release/flatshot /usr/bin/flatshot
sudo install -Dm644 packaging/flatshot.desktop /usr/share/applications/flatshot.desktop
sudo install -Dm644 packaging/flatshot-kwin.desktop /usr/share/applications/flatshot-kwin.desktop
sudo install -Dm644 assets/flatshot.notifyrc /usr/share/knotifications6/flatshot.notifyrc
sudo install -Dm644 assets/flatshot.svg /usr/share/icons/hicolor/scalable/apps/flatshot.svg
```

Or `packaging/arch/PKGBUILD` with `makepkg`, or `packaging/build-packages.sh` (with [nfpm](https://nfpm.goreleaser.com)) for all three package formats.

**On KDE, the binary must be at `/usr/bin/flatshot`** with `flatshot-kwin.desktop` installed: KWin only lets programs named (by their full path) in such a desktop file use its screenshot API. Anywhere else Flatshot falls back to the screenshot portal or Spectacle, which are slower.

## Getting started

1. Start **Flatshot** from the app menu. This runs `flatshot --tray`, and the icon appears in the system tray.
2. On KDE the shortcuts are registered for you: <kbd>Ctrl</kbd>+<kbd>Print</kbd> (region), <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Print</kbd> (active window) and <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Print</kbd> (all screens). Change them, or set keys for current monitor, last region, pin and record, on the Shortcuts page of Flatshot's settings (or in **System Settings → Shortcuts → Flatshot**).
3. On other desktops, bind the command `flatshot` (or `flatshot --full`, ...) in your desktop's keyboard settings: it hands the capture to the running tray app at once, or captures by itself when there is none.

Replacing the Python Flatshot: starting this one's tray app asks the old one to quit, and takes over its shortcuts and settings (`~/.config/flatshot/config.json` is shared).

## Using it

| | |
|---|---|
| Drag | capture region (saved and copied) |
| Click | capture the highlighted window, or the whole monitor |
| <kbd>Q</kbd> | hide / show detected QR codes and barcodes |
| <kbd>I</kbd> | copy the colour under the cursor (as `#RRGGBB`) and close |
| <kbd>K</kbd> | pin instead of save |
| <kbd>G</kbd> | snap selections to edges in the picture (hold <kbd>Ctrl</kbd> to place freely) |
| <kbd>Enter</kbd> / <kbd>Ctrl</kbd>+<kbd>S</kbd> / <kbd>Ctrl</kbd>+<kbd>C</kbd> | capture the whole monitor with your drawings |
| <kbd>Esc</kbd> / right-click | cancel the current drag, or quit |
| <kbd>R</kbd> | region tool (back to capture mode) |
| <kbd>M</kbd> | show or hide the mouse pointer in the picture |
| <kbd>P</kbd> <kbd>L</kbd> <kbd>A</kbd> <kbd>B</kbd> <kbd>F</kbd> <kbd>E</kbd> <kbd>H</kbd> <kbd>T</kbd> <kbd>X</kbd> <kbd>U</kbd> <kbd>N</kbd> | pen, line, arrow, box, filled box, ellipse, highlighter, text, pixelate, blur, counter |
| <kbd>Shift</kbd> / <kbd>Ctrl</kbd> / <kbd>Alt</kbd> while drawing | square / from the middle / move without resizing |
| <kbd>1</kbd>–<kbd>7</kbd>, <kbd>8</kbd> | colour, your own colour |
| <kbd>[</kbd> <kbd>]</kbd> | stroke size |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Z</kbd> | undo / redo |

### Recording the screen

Choose **Record screen** in the tray menu (or its shortcut, or <kbd>V</kbd> on the overlay, or `flatshot --record`), pick an area or a window or a whole monitor, set MP4, WebM or GIF, sound and frame rate in the panel under it, and press <kbd>Enter</kbd>. A small bar under the area (outside it, so it's never in the video) shows the time and has pause, stop and discard; red marks show the area's corners. Stop with the bar, the tray icon, the shortcut again, or <kbd>Ctrl</kbd>+<kbd>C</kbd> in the terminal: the video is saved to `~/Videos/Screencasts` (settings: Recording), copied as a file, and announced.

| | KDE Plasma, GNOME | Sway, Hyprland, wlroots |
|---|---|---|
| Records with | the screen-cast portal and GStreamer (the first time, the desktop asks which screen to share) | wf-recorder |
| Needs | `gst-launch-1.0` with the PipeWire plugin, and x264enc (MP4) or vp8enc (WebM) | `wf-recorder` |

Pausing and GIFs also need `ffmpeg`. The recorders are programs of their own: no frame passes through Flatshot.

### Command line

```
flatshot                      region capture (handed to the tray app if it runs)
flatshot -f | -m | -w | -l    all screens, current monitor, active window, last region
flatshot -r 800x600+100+50    an exact area (logical pixels)
flatshot -d 3                 after 3 seconds
flatshot -o file.png          save there ("-" writes PNG to stdout)
flatshot --no-copy --no-save  just print the path / don't keep a file
flatshot --record             record an area (stops the recording in progress)
flatshot -i picture.png       annotate a picture
flatshot --settings           the settings window
flatshot --tray               the tray app
flatshot --quit               stop the tray app
flatshot -v                   how long each step takes, on stderr
```

Exit codes: 0 captured, 1 cancelled, 2 failed.

## How it works

| | KDE Plasma | Sway, Hyprland, wlroots | GNOME |
|---|---|---|---|
| Taking the screen | KWin's ScreenShot2 API, raw pixels over a pipe | wlr-screencopy, into shared memory | the screenshot portal |
| The overlay | a layer-shell surface (no window animations) | layer-shell | a full-screen window |
| Window detection | a KWin script | sway IPC / Hyprland IPC | — |
| Shortcuts | KGlobalAccel | your compositor's key bindings | your desktop's key bindings |
| Clipboard | data-control (after the overlay is gone) | data-control | while the overlay has the keyboard |

Change settings in the settings window (the tray menu's **Settings…**, or `flatshot --settings`): they apply from the next capture. They live in `~/.config/flatshot/config.json` (the same file and keys as the Python Flatshot); `flatshot --print-config` shows them all.

## Security

- **Untrusted input is parsed by memory-safe code**: the barcode decoders (rxing), image decoders, the compositor's and D-Bus messages. A decoder that panics on a strange picture is caught; the capture goes on without codes.
- **Files**: captures are created with `O_EXCL | O_NOFOLLOW` under a unique name, so a capture never replaces a file or writes through a planted link; temporary files and the KWin script live in a private (0700) runtime folder and are created exclusively, mode 0600.
- **IPC**: the tray app's socket is in a folder only you can enter, and both ends check the other's user ID (`SO_PEERCRED`). Commands are a fixed, small grammar; lines are capped.
- **D-Bus**: replies from KWin's window script are only accepted from KWin's own connection; shortcut presses only from the shortcut service's, notification clicks only from the notification server's. Window lists and titles are capped in size.
- **What the screen says stays text**: code contents and window titles shown in notifications are escaped, with invisible direction overrides replaced, so a QR code can't pose as something else; "Open" is only offered for http(s), mailto, tel, sms, geo and matrix links.
- **Commands you configure** ("run after capture") get the file's path quoted for the shell, and in `$FLATSHOT_PATH`.

## Development

```sh
cargo test                                 # unit tests
cargo run -- --self-test                   # the overlay, headless
cargo build --examples && tests/e2e.sh     # end to end on a headless sway
```

The examples include stand-ins for KWin's screenshot API, KDE's shortcut service and tray, and a notification server, so the KDE paths can be tested without Plasma.

## License

MIT
