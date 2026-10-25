# Flatshot RS

A ShareX-style screenshot tool for Linux, on Wayland and X11, built mainly for KDE Plasma: a rewrite of [Flatshot](https://github.com/nicholas-sbx/screenshot) as a single native binary, with no Python, Qt or helper programs. It installs and runs as `flatshot-rs`, so it can sit alongside the Python Flatshot (see [Alongside the Python Flatshot](#alongside-the-python-flatshot)).

Flatshot RS lives in your system tray. Press your shortcut and the screen freezes. **Drag a region, or click a window, and it's saved and copied straight away.** To mark it up first, draw on the frozen screen with the floating toolbar, then drag. **QR codes and barcodes** on screen are found automatically, each with a card to copy or open it.

## Why the rewrite

- **Faster to open.** The tray app keeps its connection to the compositor and everything it needs loaded. A shortcut press goes straight from KDE's shortcut service to a capture: KWin's screenshot API writes the raw pixels into Flatshot's memory, the overlay's surfaces are set up while that happens, and the first frame is painted as soon as the pixels are in. No process starts, nothing is encoded or decoded on the way. Run `flatshot --tray -v` to see each step's time.
- **Less memory.** No interpreter or GUI toolkit: an idle tray app is a few megabytes, and every capture gives its memory back when it closes. Pictures are shared with the compositor, not copied; drawing repaints only what changed.
- **Safer.** Memory-safe code throughout, including the QR and barcode decoders, which read whatever is on screen. Files are written without following links or replacing anything that exists; the IPC socket, scripts and temporary files are private to you; text from the screen (code contents, window titles) is shown as plain text, never as markup; only KDE's own services are listened to for shortcuts and notification clicks. See [Security](#security).

### Measured

Both versions on the same machine and desktop, 1600 × 1000, software rendering, from the moment of the key press, or of starting the program, to the overlay's first frame on screen, and the tray app's memory. On Wayland (a headless Sway; times from the Wayland protocol trace):

| | Flatshot (Python) | Flatshot (Rust) |
|---|---|---|
| Shortcut → overlay on screen, tray app running | 100 ms (87–124) | **19 ms** (18–20) |
| `flatshot-rs` → overlay on screen, no tray app | 324 ms (303–350) | **24 ms** (22–30) |
| Tray app, idle | 65 MB | **12 MB** |
| Tray app, after six captures | 93 MB | **16 MB** |
| Peak during a capture | 124 MB | **40 MB** |

On X11 (Xvfb with Openbox; times from the X server's own damage reports):

| | Flatshot (Python) | Flatshot (Rust) |
|---|---|---|
| Shortcut → overlay on screen, tray app running | 74 ms (66–88) | **17 ms** (16–18) |
| `flatshot-rs` → overlay on screen, no tray app | 296 ms (282–310) | **21 ms** (18–23) |
| Tray app, idle | 61 MB | **10 MB** |
| Tray app, after six captures | 79 MB | **14 MB** |
| Peak during a capture | 107 MB | **38 MB** |

Most of the Rust version's time is the display server copying the screen. (Memory is resident set size; times are means of 7 runs, ranges in brackets. The shortcut is KDE's shortcut service, a stand-in that notes when it fires.)

## Install

Each release has packages for Debian/Ubuntu (`.deb`), Fedora/openSUSE (`.rpm`) and Arch (`.pkg.tar.zst`):

| Distro | Install |
|---|---|
| Debian 12+, Ubuntu 22.04+, KDE neon | `sudo apt install ./flatshot-rs_*_amd64.deb` |
| Fedora / openSUSE | `sudo dnf install ./flatshot-rs-*.x86_64.rpm` |
| Arch / Manjaro / EndeavourOS | `sudo pacman -U flatshot-rs-*-x86_64.pkg.tar.zst`, or the pacman repository below |

### Arch: pacman repository

Each release also updates a pacman repository, so Flatshot RS updates with the rest of your system:

```sh
sudo tee -a /etc/pacman.conf <<'EOF'

[flatshot-rs]
SigLevel = Optional TrustAll
Server = https://github.com/nicholas-sbx/screenshot-rs/releases/download/pacman
EOF
sudo pacman -Sy flatshot-rs
```

`sudo pacman -Syu` then picks up new versions. To remove it, delete the `[flatshot-rs]` block and `sudo pacman -R flatshot-rs`.

- **The repository has to be public** for pacman to download from it: GitHub only serves a private repository's releases to people signed in.
- The packages are signed when the release workflow has a `PACMAN_GPG_KEY` secret; until then `SigLevel = Optional TrustAll` accepts them unsigned.

### From source

Rust 1.89 or newer and libxkbcommon (with its headers):

```sh
cargo build --release
sudo install -Dm755 target/release/flatshot-rs /usr/bin/flatshot-rs
sudo install -Dm644 packaging/flatshot-rs.desktop /usr/share/applications/flatshot-rs.desktop
sudo install -Dm644 packaging/flatshot-rs-kwin.desktop /usr/share/applications/flatshot-rs-kwin.desktop
sudo install -Dm644 assets/flatshot-rs.notifyrc /usr/share/knotifications6/flatshot-rs.notifyrc
sudo install -Dm644 assets/flatshot-rs.svg /usr/share/icons/hicolor/scalable/apps/flatshot-rs.svg
```

Or `packaging/arch/PKGBUILD` with `makepkg`, or `packaging/build-packages.sh` (with [nfpm](https://nfpm.goreleaser.com)) for all three package formats.

**On KDE, the binary must be at `/usr/bin/flatshot-rs`** with `flatshot-rs-kwin.desktop` installed: KWin only lets programs named (by their full path) in such a desktop file use its screenshot API. Anywhere else Flatshot RS falls back to the screenshot portal or Spectacle, which are slower.

**After an upgrade, restart the tray app** (tray menu → **Restart Flatshot RS**, or log out and in). The running one is still the old binary, which the upgrade deleted, and KWin no longer recognises it as `/usr/bin/flatshot-rs`. So captures go through the screenshot portal until it restarts: KDE asks for permission to take screenshots, the pointer is always in the picture, and captures are slower. This happens with any upgrade of Flatshot RS, including one in the same `pacman -Syu` as a Plasma update.

## Getting started

1. Start **Flatshot RS** from the app menu. This runs `flatshot-rs --tray`, and the icon appears in the system tray.
2. On KDE the shortcuts are registered for you: <kbd>Ctrl</kbd>+<kbd>Print</kbd> (region), <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Print</kbd> (active window) and <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Print</kbd> (all screens). Change them, or set keys for current monitor, last region, pin and record, on the Shortcuts page of its settings (or in **System Settings → Shortcuts → Flatshot RS**).
3. On other desktops, bind the command `flatshot-rs` (or `flatshot-rs --full`, ...) in your desktop's keyboard settings: it hands the capture to the running tray app at once, or captures by itself when there is none. The same goes for X11 sessions; KDE's X11 session registers the shortcuts like its Wayland one. Where the desktop's portal offers global shortcuts (GNOME 48 and later, Hyprland), the Shortcuts page of the settings can instead ask it for them (**Ask the desktop for these**): GNOME then asks you to confirm or change the keys, and on Hyprland you bind keys to them in its config with the `global` dispatcher (`hyprctl globalshortcuts` lists their names). It's off until you turn it on, and never used where KDE's shortcut service is.

### Alongside the Python Flatshot

Both can be installed and running at once: nothing is shared. This one is `flatshot-rs` everywhere the Python one is `flatshot`: the program, the packages, the desktop files and icon, the settings (`~/.config/flatshot-rs/`), the tray app's socket, and KDE's shortcut and notification components (**Flatshot RS** in System Settings). The first time it runs, it starts from a copy of the Python Flatshot's settings, if there are any; from then on each keeps its own.

Both come with the same default keys. KDE gives a key to whichever registered it first: change one of them in System Settings → Shortcuts.

## Using it

| | |
|---|---|
| Drag | capture region (saved and copied); <kbd>Shift</kbd> / <kbd>Ctrl</kbd> / <kbd>Alt</kbd>: square / from the middle / move it, as when drawing |
| Click | capture the highlighted window, or the whole monitor |
| <kbd>Q</kbd> | hide / show detected QR codes and barcodes |
| <kbd>I</kbd> | copy the colour under the cursor (as `#RRGGBB`) and close |
| <kbd>K</kbd> | pin instead of save |
| <kbd>G</kbd> | snap selections to edges in the picture (hold <kbd>Ctrl</kbd> to place freely) |
| <kbd>Enter</kbd> / <kbd>Ctrl</kbd>+<kbd>S</kbd> / <kbd>Ctrl</kbd>+<kbd>C</kbd> | capture the whole monitor with your drawings |
| <kbd>Esc</kbd> / right-click | cancel the current drag, or quit |
| <kbd>R</kbd> | region tool (back to capture mode) |
| <kbd>M</kbd> | show or hide the mouse pointer in the picture |
| <kbd>V</kbd>, then <kbd>Ctrl</kbd> while resizing the area | record tool; resize the area about its centre |
| <kbd>O</kbd> | copy text: drag over text (or click a window) to copy the text in it ([OCR](#copying-text-from-the-screen)) |
| <kbd>S</kbd> | select tool: click a drawing to move, resize, restyle or delete it (<kbd>Delete</kbd>, arrow keys to nudge) |
| <kbd>Ctrl</kbd>+<kbd>B</kbd> | Beautify: put this and later captures on a background |
| <kbd>P</kbd> <kbd>L</kbd> <kbd>A</kbd> <kbd>B</kbd> <kbd>F</kbd> <kbd>E</kbd> <kbd>H</kbd> <kbd>T</kbd> <kbd>X</kbd> <kbd>U</kbd> <kbd>D</kbd> <kbd>Z</kbd> <kbd>N</kbd> | pen, line, arrow, box, filled box, ellipse, highlighter, text (again: on a label, then a callout), pixelate, blur, spotlight, magnifier, counter |
| <kbd>Shift</kbd> / <kbd>Ctrl</kbd> / <kbd>Alt</kbd> while drawing | square / from the middle / move without resizing |
| <kbd>1</kbd>–<kbd>9</kbd>, <kbd>0</kbd>, <kbd>Alt</kbd>+<kbd>1</kbd>–<kbd>3</kbd> | the toolbar's colours from the left, then your own (<kbd>8</kbd> with the default seven colours) |
| <kbd>[</kbd> <kbd>]</kbd> | stroke size |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Z</kbd> | undo / redo |

The keys can be changed in Settings → Shortcuts → Keys in Flatshot RS (so can the editor's: crop <kbd>C</kbd>, background <kbd>Ctrl</kbd>+<kbd>B</kbd>, fit <kbd>Ctrl</kbd>+<kbd>0</kbd>, zoom <kbd>Ctrl</kbd>+<kbd>=</kbd> / <kbd>-</kbd>, ...).

### Drawing and editing

- **Select, move and resize drawings** (<kbd>S</kbd>, on the toolbar and in the editor): click a drawing to select it (<kbd>Alt</kbd>+click for the one under it), drag it to move it (<kbd>Shift</kbd>: straight across or down), drag a handle to resize it (<kbd>Shift</kbd> keeps its shape, <kbd>Ctrl</kbd> from the middle; lines and arrows by their ends), nudge it with the arrow keys (<kbd>Shift</kbd>: 10 px), or press <kbd>Delete</kbd> (or its trash button, for a finger). The colour and size buttons restyle it. Every change can be undone.
- **Your own toolbar colours** (Settings → Capture → Toolbar colours): add up to 12, remove them, change each with a colour picker or its hex code, or reset to the default palette. The toolbar grows or shrinks to fit them, in the capture screen and the editor. Where it's too wide for the screen (a narrow or upright monitor, many colours), the capture toolbar comes in two rows or more, and the editor's in two while its window is that narrow. Every swatch has a key, in toolbar order: <kbd>1</kbd>–<kbd>9</kbd>, <kbd>0</kbd>, then <kbd>Alt</kbd>+<kbd>1</kbd>–<kbd>3</kbd>, with your own colour on the key after your last colour (<kbd>8</kbd> with the default seven).
- **Pixelate and blur** hide what's under them from reading, and resist the tools that read pixelated text back. Those tools render guesses in the same font, pixelate them alike and keep the closest; that works on plain mosaics because each block is exactly the average of its own pixels. Here each block, and each point of the blur, is taken from a window shifted a little at random, blends into its neighbours, and gets fine random grain where there's detail (flat colour stays clean). In a test where a 4-digit PIN is pixelated and all 10,000 PINs are tried this way, a plain mosaic or blur gives the PIN away first every time; these leave it somewhere in the middle. For passwords and keys, the filled rectangle (<kbd>F</kbd>) is still the only sure way: nothing of what's under it is kept.
- **Text**: click to type; <kbd>Enter</kbd> starts a new line; <kbd>Esc</kbd>, <kbd>Ctrl</kbd>+<kbd>Enter</kbd> or a click elsewhere finishes (with the text tool, that click starts the next text there). A text left blank goes. Drag across the text to select some of it. Click text you've placed to change it; the select tool moves it.
- **Labels and callouts**: choose the text tool again (its button, or <kbd>T</kbd>) for text on a label, white or black on a rounded box of your colour, and once more for a callout: a label with a tail. Drag from what it points at to where its text goes (or just click, and the text goes above and to the right), then type. Moved, the tail goes with it.
- **Spotlight** (<kbd>D</kbd>): drag over what matters and everything else is shaded. Several spotlights light up together, overlapping or not; drawings made after them aren't shaded.
- **Magnifier** (<kbd>Z</kbd>): drag out a box and what's under its middle shows in it enlarged, framed in your colour (2×, 3× or 4× with the stroke size), crisp pixel for pixel.
- **Across monitors**: drawings can run from one monitor onto another with every tool: they show on each, stacked alike (the last drawn or changed on top), the select tool picks them up (and frames and resizes them) from either, and text can be typed and changed from either. The toolbar follows the pointer from monitor to monitor, during a drag too. Edges snap on whichever monitor a drag reaches. A window across monitors is outlined on each, with its label on the one you're pointing at. A pin of an area across monitors shows on each, and pins and the recording bar can be dragged onto any monitor.
- **The annotation editor** (**Annotate** on a notification, in the tray menu, or `flatshot-rs -i picture.png`; **Annotate clipboard image** in the tray menu, or `flatshot-rs --from-clipboard`, for the picture you copied) has the same tools, and:
  - **The picture you copied is in no file until you save it**: Save asks where, starting in your captures' folder with a capture's name (on a desktop without a file dialog, it's saved there). `flatshot-rs --from-clipboard -o file.png` names the file up front.
  - **Crop, cut out, rotate and resize** (<kbd>C</kbd>): drag the frame's edges in to crop, or out past the picture for a margin (clear, or your colour), then <kbd>Enter</kbd>. The crop can be free, 1:1, 4:3, 16:9 or the original's shape, and snaps to edges in the picture like the magnet. Under the toolbar: cut out rows or columns (drag across them and the rest is joined), rotate, flip, and resize to a size in pixels or a percentage. Your drawings move with the picture and stay drawings, and each step can be undone.
  - **Beautify** (<kbd>Ctrl</kbd>+<kbd>B</kbd>): the picture on a background (a colour, its own edge colour, a gradient, a blurred copy of itself or an image), with padding, rounded corners, a soft shadow and a fixed shape (1:1, 4:3, 16:9) if you like, shown as you work and saved at full resolution. Save styles as presets. The capture toolbar's Beautify button (<kbd>Ctrl</kbd>+<kbd>B</kbd> there too), or Settings → After capture → Beautify, puts every capture on the style you used last.
  - The zoom is at the bottom right: out and in, a slider, the zoom itself (click it for a list) and fit. **Save** is highlighted while there are unsaved changes, greyed out when there are none. The window can't be made so small that its bars overlap.

### Captures and the tray

- **The active window on KDE** is KWin's picture of the window itself: nothing covering it shows up, parts off screen are kept, and its rounded corners and shadow come out transparent (JPEG fills them in white). Settings → Capture → Window capture turns the title bar and borders, and the shadow, on or off. Clicking a window in the overlay still takes it from the frozen screen, as you see it.
- **Window capture** includes panels, docks, notifications, menus and tooltips (Settings → Capture → Include panels and popups), on KDE and X11. Flatshot RS's own windows (the editor, the settings, pins) are in the picture and can be picked like any other; only the capture overlay itself is left out.
- **QR codes** are looked for while they're shown (<kbd>Q</kbd>); hidden at the start, the capture isn't looked through until you show them.
- **Tray icon styles** (Settings → General → Tray icon): the app's colours, your theme's, a plain white or black outline for panels with symbolic icons, or Auto (white or black, following the desktop's light or dark scheme). The tray menu has every capture with its key, **Restart Flatshot RS** (after an upgrade) and **Quit Flatshot RS**.

### Delayed captures

**Capture region in 3 seconds** and **Capture active window in 3 seconds** in the tray menu, and `flatshot-rs -d SECONDS`, count down on screen: a small dial at the bottom of each monitor shows the seconds left, with a ring running down, and lets what's under it show through. It never takes the keyboard or the pointer (clicks go through it to what's under it), so a menu you open for the capture stays open, and it's gone a moment before the picture is taken, so it's never in it.

### Recent captures

**Recent captures…** in the tray menu (or `flatshot-rs --recent`) opens a window of the last 100 captures, recordings and pictures saved from the editor, newest first. Each card shows the capture and what's known about it: what was captured (region, window, monitor, all screens, recording and its length), the window it came from, its size in pixels and on disk, when, and whether it was copied, had QR codes in it, drawings on it, or was edited since. Point at one (or choose it with the arrow keys) to open it (or double-click, or <kbd>Enter</kbd>), annotate it, copy it (<kbd>Ctrl</kbd>+<kbd>C</kbd>: the picture, or a recording's file), show it in its folder, or take it off the list (<kbd>Delete</kbd>). A file that has since gone is marked so.

Only the list is kept (`~/.local/state/flatshot-rs/recent.jsonl`, private to you), never another copy of a capture: the cards' pictures are made from the files themselves while the window is open, only for the cards in view, and let go when it closes. Taking a capture off the list, or clearing it, leaves its file where it is.

### Recording the screen

Choose **Record screen** in the tray menu (or its shortcut, or <kbd>V</kbd> on the overlay, or `flatshot-rs --record`), pick an area or a window or a whole monitor, set MP4, WebM or GIF, sound and frame rate in the panel under it, and press <kbd>Enter</kbd>. Drawings made before choosing it are out of sight meanwhile (a video never has any) and come back with any other tool. Until then you can drag another area or click another window on any monitor; an area across monitors has its handles on each, so it's moved and resized from either, and the panel goes with most of it. The frame rate **Native** records as often as the monitor shows a new picture (144 fps on a 144 Hz monitor; across monitors, the fastest), at a steady rate as players and editors expect, a still screen costing next to nothing; where the monitor doesn't say, the frames as the screen gives them.

The overlay goes as soon as you press <kbd>Enter</kbd>, and the countdown (Settings → Recording, 3 seconds unless you change it) runs on the desktop itself: the keyboard and pointer are yours, to get the windows you're recording ready. A dial over the middle of the area counts down (it lets what's under it show through, and clicks go through it), red marks show the area's corners, and the bar under the area is one button, **Cancel** (where the desktop can't place a dial, as on GNOME, it shows the seconds); the tray icon or the shortcut again cancel it too. The first time, the desktop asks which screen to share meanwhile. Once it records, the bar (the same size: nothing moves) shows the time and has pause, stop and discard, outside the area so it's never in the video. Stop with the bar, the tray icon, the shortcut again, or <kbd>Ctrl</kbd>+<kbd>C</kbd> in the terminal: the video is saved to `~/Videos/Screencasts` (settings: Recording), copied as a file, and announced.

| | KDE Plasma, GNOME | Sway, Hyprland, wlroots | X11 |
|---|---|---|---|
| Records with | the screen-cast portal and GStreamer (the first time, the desktop asks which screen to share) | wf-recorder | ffmpeg (x11grab) |
| Needs | `gst-launch-1.0` with the PipeWire plugin, and x264enc or openh264enc (MP4) or vp8enc (WebM) | `wf-recorder` | `ffmpeg` |

Pausing and GIFs also need `ffmpeg`. The recorders are programs of their own: no frame passes through Flatshot RS.

**Keeping up with fast monitors.** With GStreamer, MP4s are encoded on the GPU when there's an encoder for it that works: `vah264enc` (VA-API: AMD, Intel; in gst-plugins-bad, with the GPU's VA-API driver) or `nvh264enc` (NVIDIA). It keeps up at any frame rate and leaves the CPU to what you're recording. Should it fail to start, the recording goes on with x264 on the CPU, for the rest of the session (`FLATSHOT_GPU_ENCODER=none` never tries it). On the CPU, the colours are converted on several cores, frames beyond the rate chosen are dropped before they cost anything, and x264 takes the best preset this CPU keeps up with at the area's size and frame rate, at a constant quality, so a still screen costs next to nothing.

### Copying text from the screen

The **copy text** tool (<kbd>O</kbd>, beside capture and record on the toolbar; **Copy text from screen** in the tray menu and as a shortcut; `flatshot-rs --ocr`) reads the text in an area: drag over it, click a window, or press <kbd>Enter</kbd> for the whole monitor. The text is copied, printed on stdout, and shown in a notification. It reads the screen as it is, without your drawings or the pointer: drawings made before choosing the tool are out of sight while it's chosen, and come back with any other tool.

The reading is done by [Tesseract](https://github.com/tesseract-ocr/tesseract) (install `tesseract` and its English data: `tesseract-ocr-eng` on Debian and Ubuntu, `tesseract-data-eng` on Arch, `tesseract-langpack-eng` on Fedora). It runs only while an area is read, as a program of its own, so the tool costs nothing the rest of the time. Before it reads, the area is made grey, light text on a dark theme is turned dark on light, and small text is enlarged to the size Tesseract reads best, which on screen text at 100% scale gets nearly every character right. Other languages: Settings → Capture → Copying text (Tesseract's codes, such as `eng+deu`, each with its language data installed).

### Command line

```
flatshot-rs                      region capture (handed to the tray app if it runs)
flatshot-rs -f | -m | -w | -l    all screens, current monitor, active window, last region
flatshot-rs -r 800x600+100+50    an exact area (logical pixels)
flatshot-rs -d 3                 after 3 seconds, counted down on screen
flatshot-rs -o file.png          save there ("-" writes PNG to stdout)
flatshot-rs --no-copy --no-save  just print the path / don't keep a file
flatshot-rs --record             record an area (stops the recording in progress)
flatshot-rs --ocr                copy the text in an area (needs tesseract)
flatshot-rs -i picture.png       annotate a picture
flatshot-rs --from-clipboard     annotate the picture on the clipboard (Save asks where)
flatshot-rs --settings           the settings window
flatshot-rs --recent             the recent captures
flatshot-rs --tray               the tray app
flatshot-rs --quit               stop the tray app
flatshot-rs -v                   how long each step takes, on stderr
```

Exit codes: 0 captured, 1 cancelled, 2 failed.

## How it works

| | KDE Plasma | Sway, Hyprland, wlroots | GNOME | X11 (any desktop) |
|---|---|---|---|---|
| Taking the screen | KWin's ScreenShot2 API, raw pixels over a pipe | wlr-screencopy, into shared memory | the screenshot portal | MIT-SHM, straight into shared memory |
| The overlay | a layer-shell surface (no window animations) | layer-shell | a full-screen window | a window per monitor the window manager never sees, holding the keyboard and pointer |
| Window detection | a KWin script | sway IPC / Hyprland IPC (every window on the workspaces shown) | — | what the window manager publishes (EWMH) |
| Shortcuts | KGlobalAccel | your compositor's key bindings, or the portal's (Hyprland, if you turn it on) | your desktop's key bindings, or the portal's (GNOME 48 and later, if you turn it on) | KGlobalAccel on KDE, else your desktop's key bindings |
| Clipboard | data-control (after the overlay is gone) | data-control | while the overlay has the keyboard | the CLIPBOARD selection (big pictures in pieces) |
| Tray icon | StatusNotifierItem | StatusNotifierItem | StatusNotifierItem (with the AppIndicator extension) | StatusNotifierItem, else the panel's tray (XEmbed) |

On X11 the scale is Xft.dpi's, in whole steps. A one-off capture that copies hands the copy to the desktop's clipboard manager, or to `xclip`; with neither it stays in the background (up to 10 minutes, until something else is copied) to serve it, as the Python version does.

Change settings in the settings window (the tray menu's **Settings…**, or `flatshot-rs --settings`): they apply from the next capture. They live in `~/.config/flatshot-rs/config.json` (the same keys as the Python Flatshot's, whose settings are copied the first time); `flatshot-rs --print-config` shows them all.

## Security

On X11 any program can read the screen and the keyboard: that's X11's design, which Wayland was made to end. What follows is what Flatshot itself does.

- **Untrusted input is parsed by memory-safe code**: the barcode decoders (rxing), image decoders, the compositor's, the X server's and D-Bus messages (no libwayland, no Xlib or libxcb: pure Rust on the wire). A decoder that panics on a strange picture is caught; the capture goes on without codes. The parsers, the decoders and the text handling are fuzzed (see [fuzz/](fuzz/README.md)), weekly in the CI too.
- **Files**: captures are created with `O_EXCL | O_NOFOLLOW` under a unique name, so a capture never replaces a file or writes through a planted link; temporary files and the KWin script live in a private (0700) runtime folder and are created exclusively, mode 0600. The editor saves into the folder it opened the picture from (held open since), replacing the file by rename: a link put in its place meanwhile is replaced, not followed. File names made from window titles lose control characters and the invisible ones that turn text around ("photo‮gnp.exe").
- **IPC**: the tray app's socket is in a folder only you can enter, and both ends check the other's user ID (`SO_PEERCRED`). Commands are a fixed, small grammar; lines are capped.
- **D-Bus**: replies from KWin's window script are only accepted from KWin's own connection; shortcut presses only from the shortcut service's (or the portal's, for the session it gave), notification clicks only from the notification server's (its current owner, followed through the bus's own announcements). Nothing waits on the bus while its messages come in, so a flood of forged ones can't stall it. Window lists and titles are capped in size.
- **What the screen says stays text**: code contents and window titles shown in notifications are escaped, with invisible direction overrides replaced, so a QR code can't pose as something else; "Open" is only offered for http(s), mailto, tel, sms, geo and matrix links.
- **Commands you configure** ("run after capture"): `{path}` becomes a reference to `$FLATSHOT_PATH`, quoted to fit where it stands, so the file's name (which may come from a window title) is never read by the shell.
- **Programs Flatshot starts** are run by their full path from `$PATH` (never the current folder) and inherit nothing but stdin, stdout and stderr: not the screen-cast stream, nor any other descriptor. Text printed to a terminal (a code's text, file names) has its control characters replaced.

## Development

```sh
cargo test                                 # unit tests
cargo run -- --self-test                   # the overlay, headless
cargo build --examples && tests/e2e.sh     # end to end on a headless sway
cargo build --examples && tests/e2e-x11.sh # ... and on a headless X server
```

The examples include stand-ins for KWin's screenshot API, KDE's shortcut service and tray, the portal's global shortcuts, and a notification server, so the KDE paths (and the portal's) can be tested without Plasma.

## License

MIT
