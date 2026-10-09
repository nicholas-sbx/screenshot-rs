#!/usr/bin/env bash
# End-to-end checks on a headless sway: captures driven by a virtual
# pointer and keyboard, instant captures, the clipboard, and the tray app
# against stand-ins for KDE's shortcut service, tray and notifications.
#
# Needs sway, wl-clipboard and dbus (dbus-run-session); run from the
# repository root after `cargo build --examples`. BIN picks the flatshot
# binary (default target/debug/flatshot).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/flatshot}
EX=target/debug/examples
work=$(mktemp -d)
export XDG_RUNTIME_DIR=$work/run XDG_CONFIG_HOME=$work/config XDG_STATE_HOME=$work/state XDG_DATA_HOME=$work/data HOME=$work/home
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/flatshot" "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"
shots=$work/shots
cat > "$XDG_CONFIG_HOME/flatshot/config.json" <<JSON
{"save_dir": "$shots", "notify": true, "clipboard": "image", "scan_codes": true}
JSON
printf 'output HEADLESS-1 resolution 1600x1000 scale 1\nseat seat0 fallback true\n' > "$work/sway.conf"

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
pass() { echo "ok: $*"; }
# The size of a PNG: "W H".
png_size() { python3 -c 'import struct,sys; d=open(sys.argv[1],"rb").read(24); print(*struct.unpack(">II", d[16:24]))' "$1"; }
newest() { ls -t "$shots"/*.png 2>/dev/null | head -n1; }
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
sleep 1
$EX/inject move 500 400 drag 100 100 300 200
for _ in $(seq 50); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
if wait $pid; then pass "region capture exited 0"; else fail "region capture exited $?"; cat "$work/region.log"; fi
expect_size "region capture" "$(newest)" "200 100"
grep -q "first frame" "$work/region.log" && pass "overlay drew its first frame" || fail "no first frame"
if [ "$(wl-paste --list-types 2>/dev/null | head -n1)" = "image/png" ]; then pass "region copied"; else fail "region not on the clipboard"; fi

# 2. Escape cancels (exit code 1).
$BIN > /dev/null 2>&1 &
pid=$!
sleep 1
$EX/inject move 300 300 key Escape
set +e
wait $pid
code=$?
set -e
[ $code = 1 ] && pass "escape cancels" || fail "escape: exit $code"

# 3. A pin: on top of the windows where the region was, dragged by the
# pointer, closed with Esc (the one-off run ends with its last pin).
$BIN --pin > /dev/null 2>&1 &
pid=$!
sleep 1
(
    $EX/inject move 500 400 drag 100 100 300 200 sleep 500 drag 150 150 450 350 sleep 1500 click 500 400 sleep 200 key Escape sleep 300
) &
injector=$!
sleep 1.6
grim "$work/pin.png" 2>/dev/null || true
wait $injector
for _ in $(seq 30); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
if kill -0 $pid 2>/dev/null; then fail "the pin didn't close"; kill $pid; else pass "pin closed with Esc"; fi
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
cp assets/flatshot.png "$work/edit.png"
before=$(md5sum < "$work/edit.png")
$BIN -i "$work/edit.png" > "$work/editor.log" 2>&1 &
pid=$!
sleep 1.2
$EX/inject move 700 500 key b drag 700 450 850 580 sleep 200 key ctrl+s sleep 400 key ctrl+w sleep 300
for _ in $(seq 30); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
if kill -0 $pid 2>/dev/null; then fail "the editor didn't close"; kill $pid; else pass "editor closed"; fi
[ "$(md5sum < "$work/edit.png")" != "$before" ] && pass "editor saved the drawing" || fail "editor didn't save"
expect_size "edited picture" "$work/edit.png" "256 256"

# 5. Instant captures.
$BIN -f > /dev/null && expect_size "all screens" "$(newest)" "1600 1000" || fail "--full failed"
$BIN -r 320x240+10+20 > /dev/null && expect_size "exact region" "$(newest)" "320 240" || fail "--region failed"
$BIN -l > /dev/null && expect_size "last region" "$(newest)" "320 240" || fail "--last-region failed"
$BIN -m > /dev/null && expect_size "current monitor" "$(newest)" "1600 1000" || fail "--monitor failed"
$BIN -r 64x48+0+0 -o "$work/out.png" > /dev/null && expect_size "--output" "$work/out.png" "64 48" || fail "--output failed"
$BIN -r 64x48+0+0 -o - --no-copy > "$work/stdout.png" && expect_size "PNG to stdout" "$work/stdout.png" "64 48" || fail "-o - failed"

# 6. The tray app, with stand-ins for KDE's services.
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
grep -q "tray: menu 1 Capture region" "$work/kde.log" && pass "tray menu" || fail "no tray menu"
grep -q "kglobalaccel: register" "$work/kde.log" && pass "shortcuts registered" || fail "shortcuts not registered"
grep -q "shortcut screen pressed" "$work/tray.log" && pass "shortcut heard" || fail "shortcut not heard"
grep -q "command: rect:100x50+0+0" "$work/tray.log" && pass "command forwarded" || fail "command not forwarded"
[ $((after - before)) -ge 3 ] && pass "tray captures saved ($((after - before)))" || fail "tray saved $((after - before)) captures"
grep -q 'actions=\["default", "Open"' "$work/notify.log" && pass "notification with buttons" || fail "no notification buttons"
grep -q "quit 0" "$work/tray-test.log" && grep -q "gone" "$work/tray-test.log" && pass "tray quits" || fail "tray didn't quit"

kill $keeper 2>/dev/null || true
if [ $failures -gt 0 ]; then
    echo "--- tray log"; cat "$work/tray.log" || true
    echo "--- kde log"; cat "$work/kde.log" || true
    echo "$failures check(s) failed"
    exit 1
fi
echo "all end-to-end checks passed"
