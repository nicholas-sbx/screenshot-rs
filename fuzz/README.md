# Fuzzing

Everything Flatshot RS reads from somewhere else, fed whatever
[libFuzzer](https://llvm.org/docs/LibFuzzer.html) can come up with: each
target checks it never crashes, and that what comes out still holds what
it should.

| Target | What it reads | What must hold |
|---|---|---|
| `decode` | picture files (PNG, JPEG, WebP, GIF, BMP): a file to annotate, the clipboard | read or refused, within the size limit |
| `codes` | anything on screen, scanned for QR codes and barcodes | the scan finishes, with sane corners |
| `textedit` | typing, the caret keys, selecting, undo and pasting in a text | the caret and the selection stay on the text's characters; one-line and length limits hold |
| `template` | a window's title and app in the folder and file name templates, and any template typed | the file stays in the folder, under a name that isn't hidden |
| `settings` | the settings file, as edited by hand or left by another version | every value read as a sane one; reading it again changes nothing |
| `history` | the list of recent captures on disk, damaged or from another version | at most the newest 100 entries, each with an absolute path |
| `keys` | key names typed in Settings or found in the settings file | a key's label reads back as the same key; any keymap answers any key press |
| `command_line` | the command line, and commands another flatshot-rs hands the tray app | parsed or refused; an exact area always has room in it |
| `windows` | the window lists KWin, sway and Hyprland report | windows with room in them, no more of them than there's room for |
| `sanitize` | text from other programs, made safe for file names, printing, notifications; Tesseract's text tidied | no slashes, control or direction characters where they mustn't be; tidying twice changes nothing |
| `beautify` | Beautify's style, any the settings file can hold | the whole picture within the size limit, drawn without a crash |
| `covers` | pixelate and blur over any part of a picture, at any scale | a cover the size of the part that's on the picture |

## Running

With a nightly Rust and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz):

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cargo +nightly fuzz run decode fuzz/corpus/decode fuzz/seeds/decode -- -max_total_time=300
```

from the repository's root (`cargo +nightly fuzz list` names the targets).
`fuzz/seeds/` holds small real inputs to start from (pictures of each kind,
a QR code and a barcode, window lists, a recent captures list);
`fuzz/corpus/` is where the inputs found are kept, and `fuzz/artifacts/`
where one that fails a check is saved, to run again with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`. The
build also needs what the app's does (`libxkbcommon-dev`).

The CI runs each target for a minute every week, and on demand.

## Found so far

- Typing in front of text a mark joins with (Arabic's number signs come
  first and make one character with the next) left the caret inside that
  character, so the next key went in before the mark.
- A control character was taken as a key's name, giving a key no label
  could show.
- A huge pointer position in KWin's report became an infinite one.
- Pixelate and blur over an area partly off the picture made a cover of
  the whole area's size from the part that's on it.
