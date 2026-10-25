#!/usr/bin/env bash
# End-to-end checks on a headless sway: captures driven by a virtual
# pointer and keyboard, instant captures, the clipboard, and the tray app
# against stand-ins for KDE's shortcut service, tray and notifications,
# the editor and settings windows, and screen recording (wf-recorder, and
# ffmpeg's and GStreamer's test patterns).
#
# Needs sway, wl-clipboard, dbus (dbus-run-session), ffmpeg and wf-recorder
# (and, for the GStreamer recorder's checks, its x264enc, h264parse and
# pipewiresrc).
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
 "record_dir": "$work/videos", "record_countdown": 0, "portal_shortcuts": true}
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

# 4b. The picture on the clipboard, annotated. It's in no file yet; with no
# file dialog to ask where it goes, it's saved where a capture would be.
wl-copy --type image/png < assets/flatshot-rs.png
before=$(ls "$shots" | wc -l)
$BIN --from-clipboard -v > "$work/clip-editor.log" 2>&1 &
pid=$!
up "$work/clip-editor.log" || true
$EX/inject move 700 500 key b drag 700 450 850 580 sleep 200 key ctrl+s sleep 400 key ctrl+w sleep 300
finish $pid 3
[ $code = 0 ] && pass "clipboard picture's editor closed" || { fail "the clipboard picture's editor didn't close (exit $code)"; tail -n 5 "$work/clip-editor.log"; }
[ $(( $(ls "$shots" | wc -l) - before )) = 1 ] && expect_size "clipboard picture saved as a capture" "$(newest)" "256 256" || fail "the clipboard picture wasn't saved in $shots"
# 4c. With a file dialog (a stand-in portal's), Save asks where, and the
# picture goes there: no name is made up for it.
wl-copy --type image/png < assets/flatshot-rs.png
before=$(ls "$shots" | wc -l)
cat > "$work/clip-dialog.sh" <<SH
#!/usr/bin/env bash
FAKE_PORTAL_SAVE="$work/chosen.png" $EX/fake_portal > "$work/clip-portal.log" 2>&1 &
sleep 0.4
$BIN --from-clipboard -v > "$work/clip-dialog.log" 2>&1 &
pid=\$!
for _ in \$(seq 150); do grep -qE "first frame|painted" "$work/clip-dialog.log" && break; sleep 0.1; done
$EX/inject move 700 500 key b drag 700 450 850 580 sleep 200 key ctrl+s sleep 800 key ctrl+w sleep 300
for _ in \$(seq 30); do kill -0 \$pid 2>/dev/null || break; sleep 0.1; done
kill \$pid 2>/dev/null
kill %1 2>/dev/null
SH
chmod +x "$work/clip-dialog.sh"
dbus-run-session -- "$work/clip-dialog.sh" 2>/dev/null || true
grep -q 'portal: save file "Save as"' "$work/clip-portal.log" && pass "Save asked where the clipboard picture goes" || fail "no Save dialog asked: $(cat "$work/clip-portal.log")"
expect_size "clipboard picture saved where the dialog said" "$work/chosen.png" "256 256"
[ "$(ls "$shots" | wc -l)" = "$before" ] && pass "nothing saved under a name of its own" || fail "a file appeared in $shots: $(ls -t "$shots" | head -n1)"
# Text on the clipboard: no picture to annotate (exit 1).
printf 'just text' | wl-copy
$BIN --from-clipboard > /dev/null 2>&1 && code=0 || code=$?
[ $code = 1 ] && pass "no picture on the clipboard: exit 1" || fail "no picture on the clipboard: exit $code"

# 5. The settings window: a change is saved at once; Esc closes it.
$BIN --settings -v > "$work/settings.log" 2>&1 &
pid=$!
up "$work/settings.log" || true
# The Capture page, then its first row's slider (screen shading) near its left end.
$EX/inject click 57 93 sleep 300 click 1360 105 sleep 300 key Escape sleep 300
finish $pid 3
[ $code = 0 ] && pass "settings closed with Esc" || fail "the settings window didn't close (exit $code)"
grep -q '"dim_opacity": 15,' "$XDG_CONFIG_HOME/flatshot-rs/config.json" && pass "setting saved" || fail "setting not saved"

# 5b. Copying text (with Tesseract): an area of the settings window's
# sidebar dragged over with --ocr, its text read and copied.
if command -v tesseract > /dev/null; then
    $BIN --settings -v > "$work/settings2.log" 2>&1 &
    settings=$!
    up "$work/settings2.log" || true
    $BIN --ocr -v > "$work/ocr.log" 2>&1 &
    pid=$!
    up "$work/ocr.log" || true
    $EX/inject move 300 300 sleep 200 drag 0 30 240 220
    finish $pid 30
    kill $settings 2>/dev/null || true
    [ $code = 0 ] && pass "--ocr exited 0" || { fail "--ocr exited $code"; cat "$work/ocr.log"; }
    text=$(wl-paste --no-newline 2>/dev/null || true)
    case "$text" in *Capture*Recording*) pass "text copied from the screen" ;; *) fail "the text copied: '$text'"; tail -n 5 "$work/ocr.log" ;; esac
else
    echo "skip: copying text (no tesseract)"
fi

# 5c. Recent captures: what was saved (the region, the picture saved from
# the editor) is listed; the window shows the list, and Esc closes it.
recent=$XDG_STATE_HOME/flatshot-rs/recent.jsonl
grep -q "$(basename "$(newest)")" "$recent" 2>/dev/null && pass "the capture is listed as recent" || fail "the capture isn't listed as recent"
grep -q '"path":"'"$work"'/edit.png".*"edited":true' "$recent" 2>/dev/null && pass "the editor's save is listed as edited" || fail "the editor's save isn't listed: $(cat "$recent" 2>/dev/null)"
$BIN --recent -v > "$work/recent.log" 2>&1 &
pid=$!
up "$work/recent.log" || true
$EX/inject move 300 300 sleep 300 key Escape sleep 300
finish $pid 3
[ $code = 0 ] && pass "recent captures window closed with Esc" || fail "the recent captures window didn't close (exit $code)"

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

# A delayed capture counts down on screen (the dial at the bottom middle),
# and the dial is gone before the picture: none of its ring is in it.
$BIN -d 2 -r 300x200+650+780 -o "$work/delayed.png" --no-copy > /dev/null 2>&1 &
pid=$!
sleep 1
grim -g "650,780 300x200" "$work/dial.png" 2>/dev/null || true
finish $pid 6
[ $code = 0 ] && pass "delayed capture exited 0" || fail "delayed capture exited $code"
ring() { # how many pixels of the dial's ring (the accent colour) in a PNG
    python3 - "$1" <<'PY'
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
raw, stride, prev, count = zlib.decompress(idat), w * bpp + 1, bytearray(w * bpp), 0
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
    count += sum(1 for x in range(w) if line[x * bpp] > 200 and 60 < line[x * bpp + 1] < 140 and line[x * bpp + 2] < 110)
    prev = line
print(count)
PY
}
[ "$(ring "$work/dial.png" 2>/dev/null || echo 0)" -gt 50 ] && pass "the countdown shows on screen" || fail "no countdown on screen ($(ring "$work/dial.png" 2>/dev/null || echo none) ring pixels)"
[ "$(ring "$work/delayed.png" 2>/dev/null || echo 1)" = 0 ] && pass "the countdown isn't in the capture" || fail "the countdown is in the capture"

# Counting down before a recording: the overlay is gone at once, so a click
# in the area goes to the desktop (and the countdown goes on); its dial is
# over the area, the bar under it one button that cancels it.
countdown() {
    python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" "$1" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["record_countdown"] = int(sys.argv[2]); json.dump(c, open(sys.argv[1], "w"))
PY
}
countdown 6
before=$(ls "$work/videos" | wc -l)
rm -f "$work/record.code"
record FLATSHOT_RECORDER=wf-recorder
grim -g "548,348 104x104" "$work/counting.png" 2>/dev/null || true
# (the overlay framed the area: halfway down its left edge, that's gone)
grim -g "294,396 10x8" "$work/area-edge.png" 2>/dev/null || true
[ "$(ring "$work/area-edge.png" 2>/dev/null || echo 1)" = 0 ] && pass "the overlay is gone while it counts down" || fail "the overlay is still up during the countdown"
$EX/inject move 600 400 sleep 100 click 600 400 sleep 400
[ ! -s "$work/record.code" ] && pass "a click in the area goes to the desktop, and it counts on" || fail "a click in the area ended the countdown"
[ "$(ring "$work/counting.png" 2>/dev/null || echo 0)" -gt 50 ] && pass "the countdown shows over the area" || fail "no countdown over the area"
$EX/inject move 600 641 sleep 200 click 600 641
for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 1 ] && [ "$(ls "$work/videos" | wc -l)" = "$before" ] && pass "the bar cancels the countdown" \
    || { fail "cancelling the countdown: exit $(cat "$work/record.code" 2>/dev/null)"; cat "$work/record.log"; }
# Once it's over, the bar is the recording's (the same size: stop is where
# it always is), and the dial isn't in the video.
countdown 2
rm -f "$work/record.code"
record FLATSHOT_RECORDER=wf-recorder
sleep 2
$EX/inject move 641 641 sleep 200 click 641 641
for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
[ "$(cat "$work/record.code" 2>/dev/null)" = 0 ] && pass "recorded after the countdown" || { fail "recording after the countdown: exit $(cat "$work/record.code" 2>/dev/null)"; cat "$work/record.log"; }
ffmpeg -v error -y -i "$(newest_video)" -frames:v 1 "$work/first-frame.png" 2>/dev/null || true
[ "$(ring "$work/first-frame.png" 2>/dev/null || echo 1)" = 0 ] && pass "the countdown isn't in the video" || fail "the countdown is in the video"
countdown 0

# GStreamer's recorder (KDE's and GNOME's), on its test pattern: told to use
# a GPU's encoder that isn't there, it records with the CPU's instead.
if command -v gst-launch-1.0 > /dev/null && gst-inspect-1.0 x264enc > /dev/null 2>&1 && gst-inspect-1.0 h264parse > /dev/null 2>&1; then
    python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["record_format"] = "mp4"; json.dump(c, open(sys.argv[1], "w"))
PY
    rm -f "$work/record.code"
    record FLATSHOT_RECORDER=test-gst FLATSHOT_GPU_ENCODER=vah264enc
    $EX/inject move 641 641 sleep 200 click 641 641
    for _ in $(seq 100); do [ -s "$work/record.code" ] && break; sleep 0.1; done
    if [ "$(cat "$work/record.code" 2>/dev/null)" = 0 ] && grep -q "so the CPU's encoder records instead" "$work/record.log"; then
        pass "the CPU's encoder records when the GPU's can't"
    else
        fail "GStreamer's recorder: exit $(cat "$work/record.code" 2>/dev/null)"
        cat "$work/record.log"
    fi
    video_check "GStreamer video" "$(newest_video)" "600x400"
    # Its picture (the notification's, the history's) is the video's first
    # frame, not the screen the area was chosen on: the pattern's yellow bar.
    thumb=$(python3 -c 'import json, sys; print(json.loads(open(sys.argv[1]).read().splitlines()[-1]).get("thumbnail", ""))' "$recent" 2>/dev/null || true)
    rgb=$(ffmpeg -v error -i "$thumb" -vf crop=1:1:8:8 -f rawvideo -pix_fmt rgb24 - 2>/dev/null | od -An -tu1 | xargs || true)
    read -r r g b <<< "${rgb:-0 0 255}"
    [ "$r" -gt 150 ] && [ "$g" -gt 150 ] && [ "$b" -lt 80 ] && pass "the recording's picture is its first frame" || fail "the recording's picture ($thumb) is $rgb there"
else
    echo "skip: GStreamer's recorder (no gst-launch-1.0, x264enc or h264parse)"
fi

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

# On KDE without KWin's screenshot API, the screen is taken another way,
# and a notification says so (unless that's turned off); none when KWin's
# API takes it. Each capture in a session of its own, with a stand-in
# notification server (and, given "kwin", KWin's API).
kde_capture() { # name, [kwin]
    cat > "$work/$1.sh" <<SH
#!/usr/bin/env bash
$EX/fake_notify > "$work/$1-notify.log" 2>&1 &
# (in an if, not after &&: the job is then the stand-in itself, which the
# kill below stops, not a subshell that would leave it running)
if [ "${2:-}" = kwin ]; then FAKE_KWIN_SCREENS=HEADLESS-1:1600x1000 $EX/fake_kwin > /dev/null 2>&1 & fi
sleep 0.4
XDG_CURRENT_DESKTOP=KDE $BIN -v -r 64x48+0+0 -o "$work/$1.png" --no-copy > "$work/$1.log" 2>&1
sleep 0.3
kill %1 %2 2>/dev/null
SH
    chmod +x "$work/$1.sh"
    dbus-run-session -- "$work/$1.sh" 2>/dev/null || true
}
fallback_notice() { # on or off
    python3 - "$XDG_CONFIG_HOME/flatshot-rs/config.json" "$1" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["notify_fallback"] = sys.argv[2] == "on"; json.dump(c, open(sys.argv[1], "w"))
PY
}
kde_capture no-kwin
expect_size "KDE without KWin's screenshot API: captured another way" "$work/no-kwin.png" "64 48"
grep -q 'summary="Screenshot taken without KWin"' "$work/no-kwin-notify.log" && grep -q "screen copy took it instead" "$work/no-kwin-notify.log" \
    && pass "a notification says KWin's screenshot API didn't take it" || fail "no notice that KWin didn't take it: $(cat "$work/no-kwin-notify.log")"
fallback_notice off
kde_capture no-kwin-quiet
fallback_notice on
expect_size "captured another way, the notice turned off" "$work/no-kwin-quiet.png" "64 48"
grep -q "without KWin" "$work/no-kwin-quiet-notify.log" && fail "told that KWin didn't take it, with that turned off" || pass "turned off, it isn't told"
kde_capture kwin-api kwin
grep -q "via kwin" "$work/kwin-api.log" && expect_size "captured by KWin's screenshot API" "$work/kwin-api.png" "64 48" || fail "KWin's stand-in didn't take it: $(grep -i screenshot "$work/kwin-api.log")"
grep -q "without KWin" "$work/kwin-api-notify.log" && fail "told that KWin didn't take it, though it did" || pass "no notice when KWin's API takes it"

# KDE's windows under the pointer: KWin's script reports them late (after
# 2.5 s, from another connection of its own); once known, a click on one
# captures it.
cat > "$work/hover.sh" <<SH
#!/usr/bin/env bash
unset SWAYSOCK
FAKE_KWIN_SCREENS=HEADLESS-1:1600x1000 $EX/fake_kwin > "$work/kwin-hover.log" 2>&1 &
sleep 0.5
XDG_CURRENT_DESKTOP=KDE $BIN -v -o "$work/hovered.png" --no-copy > "$work/hover.log" 2>&1 &
pid=\$!
for _ in \$(seq 100); do grep -q "windows known" "$work/hover.log" && break; sleep 0.1; done
$EX/inject move 400 300 sleep 200 click 400 300
for _ in \$(seq 50); do kill -0 \$pid 2>/dev/null || break; sleep 0.1; done
kill \$pid 2>/dev/null
kill %1 2>/dev/null
SH
chmod +x "$work/hover.sh"
dbus-run-session -- "$work/hover.sh" 2>/dev/null || true
grep -q "1 window" "$work/hover.log" && pass "KWin's windows known, late" || { fail "KWin's windows never came"; tail -n 15 "$work/hover.log" "$work/kwin-hover.log" | sed 's/^/    /'; }
expect_size "a click on a window captures it" "$work/hovered.png" "400 300"
# (KWin's picture, Flatshot's own windows in it: it asked KWin not to hide them)
grep -q "via kwin" "$work/hover.log" && pass "KWin's picture keeps Flatshot's own windows" || fail "the picture wasn't KWin's (did it ask KWin to hide Flatshot's windows?)"
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
# (KDE's service was there: the portal, though turned on, wasn't asked)
grep -q "from the desktop's portal" "$work/tray.log" && fail "the portal was asked beside KDE's service" || pass "KDE's shortcuts before the portal's"

# 9. Global shortcuts from the desktop's portal, without KDE's service.
cat > "$work/portal.sh" <<SH
#!/usr/bin/env bash
set -u
FAKE_PORTAL_PRESS="screen@1000" $EX/fake_portal > "$work/portal.log" 2>&1 &
sleep 0.4
$BIN --tray -v > "$work/tray-portal.log" 2>&1 &
sleep 4
$BIN --quit
sleep 1
kill %1 2>/dev/null
SH
chmod +x "$work/portal.sh"
before=$(ls "$shots" | wc -l)
dbus-run-session -- "$work/portal.sh" 2>/dev/null
after=$(ls "$shots" | wc -l)
grep -q "portal: registered flatshot-rs" "$work/portal.log" && grep -q 'portal: bind screen "Capture all screens" CTRL+SHIFT+Print' "$work/portal.log" \
    && pass "shortcuts asked of the portal" || fail "shortcuts not asked of the portal: $(cat "$work/portal.log")"
grep -q "shortcut screen pressed" "$work/tray-portal.log" && [ $((after - before)) -ge 1 ] && pass "a portal shortcut heard ($((after - before)) saved)" \
    || fail "portal shortcut not heard: $(grep -i -E "portal|shortcut" "$work/tray-portal.log")"

# 10. Recording on KDE and GNOME: the screen is asked of the desktop's portal
# as the countdown begins (which to share may be asked meanwhile); cancelled
# while it's asking, its dialog goes too.
if command -v gst-launch-1.0 > /dev/null && gst-inspect-1.0 pipewiresrc > /dev/null 2>&1 && gst-inspect-1.0 x264enc > /dev/null 2>&1; then
    countdown 5
    cat > "$work/cast.sh" <<SH
#!/usr/bin/env bash
$EX/fake_portal > "$work/cast-portal.log" 2>&1 &
sleep 0.4
FLATSHOT_RECORDER=portal $BIN --tray -v > "$work/tray-cast.log" 2>&1 &
# (the tray app records: once it's ready, --record is passed to it)
for _ in \$(seq 150); do grep -q "tray app ready" "$work/tray-cast.log" && break; sleep 0.1; done
$BIN --record > /dev/null 2>&1 &
for _ in \$(seq 150); do grep -qE "first frame|painted" "$work/tray-cast.log" && break; sleep 0.1; done
$EX/inject move 400 300 drag 300 200 900 600 sleep 400 key Return sleep 1500 move 600 641 sleep 200 click 600 641 sleep 800
# (what the portal heard by then: not what quitting the tray app does)
cp "$work/cast-portal.log" "$work/cast-cancelled.log"
$BIN --quit
sleep 1
kill %1 2>/dev/null
SH
    chmod +x "$work/cast.sh"
    dbus-run-session -- "$work/cast.sh" 2>/dev/null || true
    countdown 0
    grep -q "portal: which screen to share?" "$work/cast-cancelled.log" && pass "the screen is asked for as the countdown begins" \
        || fail "the screen wasn't asked for: $(cat "$work/cast-portal.log")"
    grep -q "portal: closed .*/request/" "$work/cast-cancelled.log" && pass "cancelling the countdown closes the portal's dialog" \
        || { fail "the portal's dialog was left up: $(cat "$work/cast-portal.log")"; tail -n 20 "$work/tray-cast.log"; }
else
    echo "skip: the screen-cast portal (no GStreamer with pipewiresrc and x264enc)"
fi

kill $keeper 2>/dev/null || true
if [ $failures -gt 0 ]; then
    echo "--- tray log"; cat "$work/tray.log" || true
    echo "--- kde log"; cat "$work/kde.log" || true
    echo "$failures check(s) failed"
    exit 1
fi
echo "all end-to-end checks passed"
