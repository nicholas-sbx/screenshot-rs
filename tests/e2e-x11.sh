#!/usr/bin/env bash
# End-to-end checks on a headless X server (Xvfb): captures driven through
# XTEST, the clipboard served to another program (big pictures in pieces),
# a pin dragged, the editor and settings windows, screen recording with
# ffmpeg's x11grab and the bar outside the area, instant captures, and the
# tray app against stand-ins for KDE's services.
#
# Needs Xvfb, ffmpeg (with x11grab) and dbus (dbus-run-session).
#
# Run from the repository root after `cargo build --bins --examples`. BIN
# picks the flatshot binary (default target/debug/flatshot-rs).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/flatshot-rs}
EX=target/debug/examples
work=$(mktemp -d)
export XDG_RUNTIME_DIR=$work/run XDG_CONFIG_HOME=$work/config XDG_STATE_HOME=$work/state XDG_DATA_HOME=$work/data HOME=$work/home
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/flatshot-rs" "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"
unset WAYLAND_DISPLAY SWAYSOCK HYPRLAND_INSTANCE_SIGNATURE
shots=$work/shots
cat > "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<JSON
{"save_dir": "$shots", "notify": true, "clipboard": "image", "scan_codes": true,
 "record_dir": "$work/videos", "record_countdown": 0}
JSON

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
pass() { echo "ok: $*"; }
png_size() { python3 -c 'import struct,sys; d=open(sys.argv[1],"rb").read(24); print(*struct.unpack(">II", d[16:24]))' "$1"; }
newest() { ls -t "$shots"/*.png 2>/dev/null | head -n1; }
up() {
    for _ in $(seq 150); do
        grep -qE "first frame|painted" "$1" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "  (nothing drawn after 15 s: $1)"
    tail -n 20 "$1" 2>/dev/null | sed 's/^/    /'
    return 1
}
finish() {
    for _ in $(seq $(( ${2:-10} * 10 ))); do kill -0 "$1" 2>/dev/null || break; sleep 0.1; done
    kill -0 "$1" 2>/dev/null && kill "$1" 2>/dev/null
    set +e
    wait "$1"
    code=$?
    set -e
}
expect_size() { # what, file, "W H"
    local got
    got=$(png_size "$2" 2>/dev/null || echo none)
    if [ "$got" = "$3" ]; then pass "$1 ($got)"; else fail "$1: $2 is $got, wanted $3"; fi
}
# The screen as it is now (another flatshot, taking it directly).
screen() { $BIN -f --no-copy -o "$1" > /dev/null 2>&1; }

# A display number nobody uses.
n=90
while [ -e "/tmp/.X11-unix/X$n" ] || [ -e "/tmp/.X$n-lock" ]; do n=$((n + 1)); done
Xvfb ":$n" -screen 0 1600x1000x24 -nolisten tcp > "$work/xvfb.log" 2>&1 &
xvfb_pid=$!
trap 'kill $xvfb_pid 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 100); do [ -e "/tmp/.X11-unix/X$n" ] && break; sleep 0.1; done
export DISPLAY=":$n"
echo "Xvfb is up on $DISPLAY"

# 1. The overlay: drag a region (not copied: the run ends as soon as it's saved).
$BIN -v --no-copy > "$work/region.log" 2>&1 &
pid=$!
up "$work/region.log" || true
$EX/xinject move 500 400 drag 100 100 300 200
finish $pid
if [ $code = 0 ]; then pass "region capture exited 0"; else fail "region capture exited $code"; cat "$work/region.log"; fi
expect_size "region capture" "$(newest)" "200 100"
grep -q "first frame" "$work/region.log" && pass "overlay drew its first frame" || fail "no first frame"

# 2. Copied: with no clipboard manager nor xclip, the run stays to serve
# the clipboard; a big picture goes in pieces (INCR).
python3 - "$work/noise.png" <<'PY'
import struct, zlib, sys, random
w, h = 700, 500
random.seed(1)
rows = b"".join(b"\x00" + bytes(random.getrandbits(8) for _ in range(w * 3)) for _ in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                              + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b""))
PY
$BIN -i "$work/noise.png" -v > "$work/noise.log" 2>&1 &
noise=$!
up "$work/noise.log" || true
$BIN -v -o "$work/copied.png" > "$work/copy.log" 2>&1 &
pid=$!
up "$work/copy.log" || true
$EX/xinject move 500 400 drag 150 120 650 470 sleep 500
kill -0 $pid 2>/dev/null && pass "the run stays to serve the clipboard" || fail "the run ended, taking the clipboard with it"
$EX/xpaste TARGETS > "$work/targets.txt" 2>/dev/null || true
grep -qx "image/png" "$work/targets.txt" && pass "the clipboard offers image/png" || fail "clipboard types: $(tr '\n' ' ' < "$work/targets.txt")"
$EX/xpaste image/png > "$work/pasted.png" 2> "$work/paste.log" || true
if cmp -s "$work/pasted.png" "$work/copied.png" && grep -q "in pieces" "$work/paste.log"; then
    pass "a big picture pasted in pieces, intact ($(stat -c %s "$work/pasted.png") bytes)"
else
    fail "paste: $(cat "$work/paste.log")"
fi
kill $pid $noise 2>/dev/null || true
wait $pid $noise 2>/dev/null || true

# 3. Escape cancels (exit code 1).
$BIN -v > "$work/escape.log" 2>&1 &
pid=$!
up "$work/escape.log" || true
$EX/xinject move 300 300 key Escape
finish $pid
[ $code = 1 ] && pass "escape cancels" || fail "escape: exit $code"

# 4. A pin: above the windows where the region was, dragged by the
# pointer, closed with Esc.
$BIN --pin -v > "$work/pin.log" 2>&1 &
pid=$!
up "$work/pin.log" || true
$EX/xinject move 500 400 drag 100 100 300 200 sleep 500 drag 150 150 450 350 sleep 500
screen "$work/pin.png"
$EX/xinject click 500 350 sleep 200 key Escape sleep 300
finish $pid 3
[ $code = 0 ] && pass "pin closed with Esc" || fail "the pin didn't close (exit $code)"
edge=$(python3 - "$work/pin.png" <<'PY'
import struct, sys, zlib
d = open(sys.argv[1], "rb").read()
w, h = struct.unpack(">II", d[16:24])
bpp = 4 if d[25] == 6 else 3
i, idat = 8, b""
while i < len(d):
    n = struct.unpack(">I", d[i:i + 4])[0]
    if d[i + 4:i + 8] == b"IDAT":
        idat += d[i + 8:i + 8 + n]
    i += 12 + n
raw, stride, prev, rows = zlib.decompress(idat), w * bpp + 1, bytearray(w * bpp), []
for y in range(h):
    f, line = raw[y * stride], bytearray(raw[y * stride + 1:(y + 1) * stride])
    for x in range(len(line)):
        a = line[x - bpp] if x >= bpp else 0
        b = prev[x]
        c = prev[x - bpp] if x >= bpp else 0
        if f == 1: line[x] = (line[x] + a) & 255
        elif f == 2: line[x] = (line[x] + b) & 255
        elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
        elif f == 4:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
    rows.append(bytes(line))
    prev = line
pts = [(x, y) for y in range(h) for x in range(w) if rows[y][x * bpp] > 160 and rows[y][x * bpp + 2] < 110 and rows[y][x * bpp + 1] < 140]
print(f"{min(p[0] for p in pts)} {min(p[1] for p in pts)} {max(p[0] for p in pts)} {max(p[1] for p in pts)}" if pts else "none")
PY
)
[ "$edge" = "400 300 599 399" ] && pass "pin dragged to its place ($edge)" || fail "pin's edge at $edge, wanted 400 300 599 399"

# 5. The annotation editor: draw a box, save it in place, close.
cp assets/flatshot-rs.png "$work/edit.png"
before=$(md5sum < "$work/edit.png")
$BIN -i "$work/edit.png" -v > "$work/editor.log" 2>&1 &
pid=$!
up "$work/editor.log" || true
$EX/xinject move 400 300 key b drag 300 250 450 380 sleep 200 key ctrl+s sleep 400 key ctrl+w sleep 300
finish $pid 3
[ $code = 0 ] && pass "editor closed" || fail "the editor didn't close (exit $code)"
[ "$(md5sum < "$work/edit.png")" != "$before" ] && pass "editor saved the drawing" || fail "editor didn't save"
expect_size "edited picture" "$work/edit.png" "256 256"

# 6. The settings window: a change is saved at once; Esc closes it.
$BIN --settings -v > "$work/settings.log" 2>&1 &
pid=$!
up "$work/settings.log" || true
# The Capture page, then its first row's slider (screen shading) near its
# left end. (No window manager: the window's top left is the screen's.)
$EX/xinject click 57 68 sleep 300 click 505 80 sleep 300 key Escape sleep 300
finish $pid 3
[ $code = 0 ] && pass "settings closed with Esc" || fail "the settings window didn't close (exit $code)"
grep -qE '"dim_opacity": [0-9],' "$XDG_CONFIG_HOME/flatshot-rs/config.json" && pass "setting saved" || fail "setting not saved: $(grep -o '"dim_opacity": [0-9]*' "$XDG_CONFIG_HOME/flatshot-rs/config.json")"

# 7. Screen recording with ffmpeg's x11grab: an area chosen on the
# overlay, stopped from the bar under it (outside the area), saved.
video_check() { # what, file, "WxH"
    local got len
    got=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x "$2" 2>/dev/null || echo none)
    len=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$2" 2>/dev/null || echo 0)
    if [ "$got" = "$3" ] && python3 -c "import sys; sys.exit(0 if float('$len' or 0) > 0.5 else 1)"; then pass "$1 ($got, ${len}s)"; else fail "$1: $2 is '$got', ${len}s"; fi
}
rm -f "$work/record.code"
(code=0; $BIN --record --no-copy -v > "$work/record.log" 2>&1 || code=$?; echo $code > "$work/record.code") &
up "$work/record.log" || true
$EX/xinject move 400 300 drag 300 200 900 600 sleep 400 key Return sleep 2000
screen "$work/recording.png"
$EX/xinject move 641 641 sleep 200 click 641 641
for _ in $(seq 150); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 0 ] && pass "recording stopped from the bar" || { fail "recording didn't end well"; cat "$work/record.log"; }
video_check "x11grab video" "$(ls -t "$work"/videos/* 2>/dev/null | head -n1)" "600x400"

# 8. Instant captures.
$BIN -f --no-copy > /dev/null && expect_size "all screens" "$(newest)" "1600 1000" || fail "--full failed"
$BIN -r 320x240+10+20 --no-copy > /dev/null && expect_size "exact region" "$(newest)" "320 240" || fail "--region failed"
$BIN -l --no-copy > /dev/null && expect_size "last region" "$(newest)" "320 240" || fail "--last-region failed"
$BIN -m --no-copy > /dev/null && expect_size "current monitor" "$(newest)" "1600 1000" || fail "--monitor failed"
$BIN -r 64x48+0+0 -o - --no-copy > "$work/stdout.png" && expect_size "PNG to stdout" "$work/stdout.png" "64 48" || fail "-o - failed"

# 9. The tray app, with stand-ins for KDE's services (as on KDE's X11 session).
cat > "$work/tray.sh" <<SH
#!/usr/bin/env bash
set -u
$EX/fake_notify > "$work/notify.log" 2>&1 &
FAKE_KDE_PRESS="screen@1500" FAKE_KDE_MENU="Capture last region@2500" $EX/fake_kde > "$work/kde.log" 2>&1 &
sleep 0.4
$BIN --tray -v > "$work/tray.log" 2>&1 &
sleep 3.5
$BIN -r 100x50+0+0 > "$work/forwarded.log" 2>&1; echo "forwarded \$?" >> "$work/tray-test.log"
sleep 1
$BIN --quit; echo "quit \$?" >> "$work/tray-test.log"
sleep 1
pgrep -f -- "$BIN --tray" > /dev/null && echo "still running" >> "$work/tray-test.log" || echo "gone" >> "$work/tray-test.log"
kill %1 %2 2>/dev/null
SH
chmod +x "$work/tray.sh"
before=$(ls "$shots" | wc -l)
dbus-run-session -- "$work/tray.sh" 2>/dev/null
after=$(ls "$shots" | wc -l)
grep -q "tray: registered" "$work/kde.log" && pass "tray icon registered" || fail "no tray icon"
grep -q "shortcut screen pressed" "$work/tray.log" && pass "shortcut heard" || fail "shortcut not heard"
grep -q "command: rect:100x50+0+0" "$work/tray.log" && pass "command forwarded" || fail "command not forwarded"
[ $((after - before)) -ge 3 ] && pass "tray captures saved ($((after - before)))" || fail "tray saved $((after - before)) captures"
grep -q "quit 0" "$work/tray-test.log" && grep -q "gone" "$work/tray-test.log" && pass "tray quits" || fail "tray didn't quit"

# 10. A panel without the D-Bus tray (stalonetray): the icon goes in it
# the old way (XEmbed), and its menu is Flatshot's own.
if command -v stalonetray > /dev/null; then
    stalonetray --geometry 1x1+700+0 --icon-size 24 > "$work/stalonetray.log" 2>&1 &
    panel=$!
    sleep 0.5
    env -u DBUS_SESSION_BUS_ADDRESS $BIN --tray -v > "$work/xembed.log" 2>&1 &
    sleep 1.5
    grep -q "embedded in the panel" "$work/xembed.log" && pass "tray icon in a panel without the D-Bus tray (XEmbed)" || fail "no XEmbed tray icon"
    before=$(ls "$shots" | wc -l)
    # Its menu (right-click), then "Capture all screens".
    $EX/xinject rclick 712 12 sleep 400 move 790 93 sleep 200 click 790 93 sleep 1500
    [ "$(ls "$shots" | wc -l)" -gt "$before" ] && pass "the tray icon's menu captures" || fail "the tray icon's menu didn't capture"
    env -u DBUS_SESSION_BUS_ADDRESS $BIN --quit
    sleep 0.5
    kill $panel 2>/dev/null || true
else
    echo "(no stalonetray: the XEmbed tray isn't checked)"
fi

if [ $failures -gt 0 ]; then
    echo "--- tray log"; cat "$work/tray.log" || true
    echo "$failures check(s) failed"
    exit 1
fi
echo "all X11 end-to-end checks passed"
