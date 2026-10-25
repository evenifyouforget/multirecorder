# multirecorder

Small app to record multiple audio streams — one track per input device, recorded
simultaneously, exported as separate WAV or FLAC files.

Built with Rust, [cpal](https://github.com/RustAudio/cpal) (WASAPI on Windows,
CoreAudio on macOS, ALSA on Linux) and [egui](https://github.com/emilk/egui).

## Using it

1. Pick an output folder and export format (FLAC 24-bit or WAV 32-bit float).
2. Tick the input devices to record, press **Record**.
3. Press **Stop**. Each device is exported to
   `<output folder>/session-YYYYMMDD-HHMMSS/NN-<device>.flac|wav`.

Each device records at its own native sample rate and channel count.

## Reliability

- Every device records on its own thread into its own file; one failing device
  never stops the others.
- All tracks share one timeline. If a device disconnects, stalls, or can't keep
  up, its track is filled with silence, and it is reopened automatically when it
  comes back. Tracks always line up and end with equal length.
- Audio is captured to headerless raw files (`*.f32`) flushed to disk every
  second, so a crash loses at most about a second. Interrupted sessions are
  listed at startup and can be exported from there.
- A 4-second buffer per device absorbs slow disk writes.
- Recording runs independently of the window (minimizing is fine).
- Every event (disconnects, reconnects, gaps) is written to `session.log` in the
  session folder.

## Building

Linux / devcontainer (build needs `libasound2-dev`; running the GUI needs
`libxkbcommon-x11-0` and an X11 or Wayland display):

```sh
cargo test
cargo run
```

Windows `.exe` from Linux (devcontainer):

```sh
sudo apt-get install gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/multirecorder.exe (standalone)
```

Natively on Windows or macOS: `cargo build --release`.

CI (`.github/workflows/ci.yml`) runs fmt/clippy, tests on Windows, macOS and
Ubuntu, and uploads release binaries as artifacts.
