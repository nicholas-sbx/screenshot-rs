#!/usr/bin/env bash
# End-to-end checks on a headless sway: captures driven by a virtual
# pointer and keyboard, instant captures, the clipboard, and the tray app
# against stand-ins for KDE's shortcut service, tray and notifications,
# the editor and settings windows, and screen recording (wf-recorder, and
# ffmpeg's test pattern).
#
# Needs sway, wl-clipboard, dbus (dbus-run-session), ffmpeg and wf-recorder.
#
# Run from the repository root after `cargo build --examples`. BIN picks the flatshot-rs
# binary (default target/debug/flatshot-rs).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/flatshot-rs}
EX=target/debug/examples
work=$(mktemp -d)
export XDG_RUNTIME_DIR=$work/run XDG_CONFIG_HOME=$work/config XDG_STATE_HOME=$work/state XDG_DATA_HOME=$work/data HOME=$work/home
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/flatshot-rs" "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"
shots=$work/shots
cat > "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<JSON
{"save_dir": "$shots", "notify": true, "clipboard": "image", "scan_codes": true,
 "record_dir": "$work/videos", "record_countdown": 0}
JSON
printf 'output HEADLESS-1 resolution 1600x1000 scale 1\nseat seat0 fallback true\n' > "$work/sway.conf"

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
pass() { echo "ok: $*"; }
# The size of a PNG: "W H".
png_size() { python3 -c 'import struct,sys; d=open(sys.argv[1],"rb").read(24); print(*struct.unpack(">II", d[16:24]))' "$1"; }
newest() { ls -t "$shots"/*.png 2>/dev/null | head -n1; }
# Wait (up to 15 s) until the program logging to $1 (run with -v) has drawn
# its first frame: input sent sooner has nowhere to go (a cold start, on a
# fresh machine, can take seconds to load its fonts).
up() {
    for _ in $(seq 150); do
        grep -qE "first frame|painted" "$1" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "  (nothing drawn after 15 s: $1)"
    tail -n 20 "$1" 2>/dev/null | sed 's/^/    /'
    return 1
}
# Wait (up to $2 s, default 10) for process $1 to end, killing it if it
# doesn't; its exit code in $code.
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

WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 sway -c "$work/sway.conf" > "$work/sway.log" 2>&1 &
sway_pid=$!
trap 'kill $sway_pid 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 100); do
    WAYLAND_DISPLAY=$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | grep -E '^wayland-[0-9]+$' | head -n1 || true)
    [ -n "$WAYLAND_DISPLAY" ] && break
    sleep 0.1
done
[ -n "$WAYLAND_DISPLAY" ] || { cat "$work/sway.log"; echo "sway didn't start"; exit 1; }
export WAYLAND_DISPLAY
echo "sway is up on $WAYLAND_DISPLAY"
# A virtual pointer and keyboard for the seat, kept for the whole run.
$EX/inject sleep 600000 &
keeper=$!
sleep 0.3

# 1. The overlay: drag a region.
$BIN -v > "$work/region.log" 2>&1 &
pid=$!
up "$work/region.log" || true
$EX/inject move 500 400 sleep 200 drag 100 100 300 200
finish $pid
if [ $code = 0 ]; then pass "region capture exited 0"; else fail "region capture exited $code"; cat "$work/region.log"; fi
expect_size "region capture" "$(newest)" "200 100"
grep -q "first frame" "$work/region.log" && pass "overlay drew its first frame" || fail "no first frame"
if [ "$(wl-paste --list-types 2>/dev/null | head -n1)" = "image/png" ]; then pass "region copied"; else fail "region not on the clipboard"; fi

# 2. Escape cancels (exit code 1).
$BIN -v > "$work/escape.log" 2>&1 &
pid=$!
up "$work/escape.log" || true
$EX/inject move 300 300 sleep 200 key Escape
finish $pid
[ $code = 1 ] && pass "escape cancels" || fail "escape: exit $code"

# 3. A pin: on top of the windows where the region was, dragged by the
# pointer, closed with Esc (the one-off run ends with its last pin).
$BIN --pin -v > "$work/pin.log" 2>&1 &
pid=$!
up "$work/pin.log" || true
(
    $EX/inject move 500 400 sleep 200 drag 100 100 300 200 sleep 500 drag 150 150 450 350 sleep 1500 click 500 400 sleep 200 key Escape sleep 300
) &
injector=$!
sleep 1.6
grim "$work/pin.png" 2>/dev/null || true
wait $injector
finish $pid 3
[ $code = 0 ] && pass "pin closed with Esc" || fail "the pin didn't close (exit $code)"
if [ -f "$work/pin.png" ]; then
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
fi

# 4. The annotation editor: draw a box, save it in place, close.
cp assets/flatshot-rs.png "$work/edit.png"
before=$(md5sum < "$work/edit.png")
$BIN -i "$work/edit.png" -v > "$work/editor.log" 2>&1 &
pid=$!
up "$work/editor.log" || true
$EX/inject move 700 500 key b drag 700 450 850 580 sleep 200 key ctrl+s sleep 400 key ctrl+w sleep 300
finish $pid 3
[ $code = 0 ] && pass "editor closed" || fail "the editor didn't close (exit $code)"
[ "$(md5sum < "$work/edit.png")" != "$before" ] && pass "editor saved the drawing" || fail "editor didn't save"
expect_size "edited picture" "$work/edit.png" "256 256"

# 5. The settings window: a change is saved at once; Esc closes it.
$BIN --settings -v > "$work/settings.log" 2>&1 &
pid=$!
up "$work/settings.log" || true
# The Capture page, then its first row's slider (screen shading) near its left end.
$EX/inject click 57 93 sleep 300 click 1360 105 sleep 300 key Escape sleep 300
finish $pid 3
[ $code = 0 ] && pass "settings closed with Esc" || fail "the settings window didn't close (exit $code)"
grep -q '"dim_opacity": 15,' "$XDG_CONFIG_HOME/flatshot-rs/config.json" && pass "setting saved" || fail "setting not saved"

# 6. Screen recording: an area chosen on the overlay, recorded by
# wf-recorder, stopped from the bar under it (or by Ctrl+C), saved.
video_check() { # what, file, "WxH"
    local got
    got=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x "$2" 2>/dev/null || echo none)
    local len
    len=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$2" 2>/dev/null || echo 0)
    if [ "$got" = "$3" ] && python3 -c "import sys; sys.exit(0 if float('$len' or 0) > 0.5 else 1)"; then pass "$1 ($got, ${len}s)"; else fail "$1: $2 is '$got', ${len}s"; fi
}
record() { # extra environment, then what to do once it records
    rm -f "$work/record.log"
    (code=0; env "$@" $BIN --record -v > "$work/record.log" 2>&1 || code=$?; echo $code > "$work/record.code") &
    up "$work/record.log" || true
    $EX/inject move 400 300 drag 300 200 900 600 sleep 400 key Return sleep 2000
}
newest_video() { ls -t "$work"/videos/* 2>/dev/null | head -n1; }
rm -f "$work/record.code"
record FLATSHOT_RECORDER=wf-recorder
# The bar is centred under the area: pause, stop, discard.
$EX/inject move 641 641 sleep 200 click 641 641
for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 0 ] && pass "recording stopped from the bar" || { fail "recording didn't end well"; cat "$work/record.log"; }
video_check "wf-recorder video" "$(newest_video)" "600x400"
rm -f "$work/record.code"
record FLATSHOT_RECORDER=wf-recorder
pkill -INT -f -- "$BIN --record" || true
for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 0 ] && pass "Ctrl+C saves the recording" || { fail "Ctrl+C: exit $(cat "$work/record.code" 2>/dev/null)"; cat "$work/record.log"; }
before=$(ls "$work/videos" | wc -l)
rm -f "$work/record.code"
record FLATSHOT_RECORDER=test
$EX/inject move 679 641 sleep 200 click 679 641
for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 1 ] && [ "$(ls "$work/videos" | wc -l)" = "$before" ] && pass "a discarded recording leaves nothing" || { fail "discard: exit $(cat "$work/record.code" 2>/dev/null)"; cat "$work/record.log"; }
python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["record_format"] = "gif"; json.dump(c, open(sys.argv[1], "w"))
PY
rm -f "$work/record.code"
record FLATSHOT_RECORDER=test
$EX/inject move 641 641 sleep 200 click 641 641
for _ in $(seq 200); do [ -s "$work/record.code" ] && break; sleep 0.1; done
case "$(newest_video)" in *.gif) video_check "GIF from the test pattern" "$(newest_video)" "600x400" ;; *) fail "no GIF made" ;; esac
ls "$HOME/.cache/flatshot-rs" 2>/dev/null | grep -q '^recording-' && fail "a recording's parts were left behind" || pass "no parts left behind"

# 7. Instant captures.
$BIN -f > /dev/null && expect_size "all screens" "$(newest)" "1600 1000" || fail "--full failed"
$BIN -r 320x240+10+20 > /dev/null && expect_size "exact region" "$(newest)" "320 240" || fail "--region failed"
$BIN -l > /dev/null && expect_size "last region" "$(newest)" "320 240" || fail "--last-region failed"
$BIN -m > /dev/null && expect_size "current monitor" "$(newest)" "1600 1000" || fail "--monitor failed"
$BIN -r 64x48+0+0 -o "$work/out.png" > /dev/null && expect_size "--output" "$work/out.png" "64 48" || fail "--output failed"
$BIN -r 64x48+0+0 -o - --no-copy > "$work/stdout.png" && expect_size "PNG to stdout" "$work/stdout.png" "64 48" || fail "-o - failed"

# Beautify after capture: on the last style's background (64 px padding).
python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["beautify"] = True; json.dump(c, open(sys.argv[1], "w"))
PY
$BIN -r 64x48+0+0 -o "$work/beautified.png" > /dev/null && expect_size "Beautify after capture" "$work/beautified.png" "192 176" || fail "beautified capture failed"
python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["beautify"] = False; json.dump(c, open(sys.argv[1], "w"))
PY

# The active window on KDE: KWin's picture of the window itself, kept with
# its transparent shadow (a stand-in KWin).
cat > "$work/window.sh" <<SH
#!/usr/bin/env bash
$EX/fake_kwin > "$work/kwin.log" 2>&1 &
sleep 0.5
XDG_CURRENT_DESKTOP=KDE $BIN -w -o "$work/window.png" --no-copy > /dev/null 2>&1
kill %1 2>/dev/null
SH
chmod +x "$work/window.sh"
dbus-run-session -- "$work/window.sh" 2>/dev/null || true
expect_size "active window from KWin" "$work/window.png" "300 200"
python3 - "$work/window.png" <<'PY' && pass "the window's shadow kept transparent" || fail "the window's shadow isn't transparent"
import sys, zlib
d = open(sys.argv[1], "rb").read()
assert d[25] == 6, "not RGBA"  # colour type
data, i = b"", 8
while i < len(d):
    n = int.from_bytes(d[i:i + 4], "big")
    if d[i + 4:i + 8] == b"IDAT":
        data += d[i + 8:i + 8 + n]
    i += 12 + n
raw = zlib.decompress(data)
assert raw[1 + 3] == 0, "corner"  # (the first pixel: zero under any of the filters)
PY
grep -q "window: cursor false decoration true shadow true" "$work/kwin.log" && pass "frame and shadow asked for" || fail "KWin asked: $(cat "$work/kwin.log")"

# 8. The tray app, with stand-ins for KDE's services.
cat > "$work/tray.sh" <<SH
#!/usr/bin/env bash
set -u
$EX/fake_notify > "$work/notify.log" 2>&1 &
FAKE_KDE_PRESS="screen@1500,monitor@6500" FAKE_KDE_MENU="Capture last region@2500" $EX/fake_kde > "$work/kde.log" 2>&1 &
sleep 0.4
$BIN --tray -v > "$work/tray.log" 2>&1 &
sleep 3.5
# Forged shortcut presses, sent straight to the tray app (a flood of them)
# and to everyone: none may count, and the tray app must keep answering.
names() { dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.\$@; }
item=\$(names ListNames | grep -o 'org.kde.StatusNotifierItem-[0-9-]*' | head -n1)
me=\$(names GetNameOwner string:\$item | grep -o '":[0-9.]*"' | tr -d '"')
forge() { dbus-send --session --type=signal "\$@" /component/flatshot_rs org.kde.kglobalaccel.Component.globalShortcutPressed string:flatshot-rs string:region int64:0; }
$EX/dbus_flood "\$me" 3000 >> "$work/tray-test.log"
forge --dest=\$me
forge
echo "forged to \$me" >> "$work/tray-test.log"
sleep 0.5
$BIN -r 100x50+0+0 > "$work/forwarded.log" 2>&1; echo "forwarded \$?" >> "$work/tray-test.log"
sleep 2.5
# Restart: the tray app starts again in its own place (the same process,
# a fresh program), serving the same socket.
ipc() { python3 -c 'import socket, sys; s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall((sys.argv[2] + "\\n").encode()); print(s.makefile().readline().strip())' "$XDG_RUNTIME_DIR/flatshot-rs/ipc" "\$1"; }
ipc restart > /dev/null
sleep 2
ipc status | grep -q '"pid"' && echo "restarted: \$(grep -c 'tray app ready' "$work/tray.log")" >> "$work/tray-test.log"
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
grep -q "tray: menu 1 Capture region" "$work/kde.log" && pass "tray menu" || fail "no tray menu"
grep -q "kglobalaccel: register" "$work/kde.log" && pass "shortcuts registered" || fail "shortcuts not registered"
grep -q "shortcut screen pressed" "$work/tray.log" && pass "shortcut heard" || fail "shortcut not heard"
if grep -q "forged to :" "$work/tray-test.log" && ! grep -q "shortcut region pressed" "$work/tray.log"; then
    pass "forged shortcut signals ignored"
    grep -q "shortcut monitor pressed" "$work/tray.log" && pass "a shortcut after the flood heard" || fail "the flood stalled the shortcuts"
else
    fail "forged shortcut signals: $(grep forged "$work/tray-test.log"), $(grep -c "shortcut region pressed" "$work/tray.log") heard"
fi
grep -q "command: rect:100x50+0+0" "$work/tray.log" && pass "command forwarded" || fail "command not forwarded"
[ $((after - before)) -ge 3 ] && pass "tray captures saved ($((after - before)))" || fail "tray saved $((after - before)) captures"
grep -q 'actions=\["default", "Open"' "$work/notify.log" && pass "notification with buttons" || fail "no notification buttons"
grep -q "restarted: 2" "$work/tray-test.log" && pass "tray restarts" || fail "tray didn't restart: $(grep restarted "$work/tray-test.log")"
grep -q "quit 0" "$work/tray-test.log" && grep -q "gone" "$work/tray-test.log" && pass "tray quits" || fail "tray didn't quit"

kill $keeper 2>/dev/null || true
if [ $failures -gt 0 ]; then
    echo "--- tray log"; cat "$work/tray.log" || true
    echo "--- kde log"; cat "$work/kde.log" || true
    echo "$failures check(s) failed"
    exit 1
fi
echo "all end-to-end checks passed"
