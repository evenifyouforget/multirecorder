# multirecorder

Small app to record multiple audio streams — one track per input device, recorded
simultaneously, exported as separate WAV or FLAC files.

Built with Rust, [cpal](https://github.com/RustAudio/cpal) (WASAPI on Windows,
CoreAudio on macOS, ALSA on Linux) and [egui](https://github.com/emilk/egui).

## Download

- Tested versions: [Releases](https://github.com/evenifyouforget/multirecorder/releases).
- Latest development build, updated on every change to `main` and checked only by
  the automated tests:
  [Windows](https://github.com/evenifyouforget/multirecorder/releases/download/dev/multirecorder-windows.exe) ·
  [macOS (Apple silicon)](https://github.com/evenifyouforget/multirecorder/releases/download/dev/multirecorder-macos.tar.gz) ·
  [Linux (x86_64)](https://github.com/evenifyouforget/multirecorder/releases/download/dev/multirecorder-linux.tar.gz)

The builds aren't code-signed. On Windows, SmartScreen may say "Windows protected
your PC": choose **More info → Run anyway**. On macOS, right-click the app and
choose **Open** the first time. The Linux build needs ALSA (`libasound2`).

## Using it

1. Pick an output folder and export format (FLAC 24-bit or WAV 32-bit float).
2. Tick the devices to record, press **Record**. Besides inputs, output devices
   (speakers, headphones) can be recorded via loopback on Windows and macOS 14.6+;
   while nothing is playing, a loopback track is filled with silence.
   **Show only selected** hides the devices you aren't using.
3. Watch the level meters and the **Warnings** column while recording. The top
   bar shows free disk space, roughly how much recording time it allows, the
   app's memory use and the busiest track's load.
4. Press **Stop**. Each device is exported to
   `<output folder>/session-YYYYMMDD-HHMMSS/NN-<device>.flac|wav`.
   Export runs alongside the recording, so Stop finishes in seconds even after
   hours. **Open last session** shows the files.

Each device records at its own native sample rate and channel count.

### Warnings

| Warning | Meaning |
|---|---|
| ⚠ choppy | Repeated short gaps of digital silence (3+ within 3 s): usually a driver, USB or buffer problem. |
| ⚠ clipping | The signal hits full scale and is distorted; lower the input gain. |
| no signal | Nothing above -80 dBFS for 5 s. Fine if the source is meant to be silent. |
| reconnecting | The device is missing; its track is being filled with silence. |

The top bar shows how many tracks need attention, and every warning is written
to `session.log` in the session folder.

The **Load** column shows how busy each track's audio callback, disk writer and
exporter are (busiest of the three). Values near 100% mean the computer can't
keep up and gaps are likely.

## Reliability

- Every device records on its own thread into its own file; one failing device
  never stops the others.
- All tracks share one timeline. If a device disconnects, stalls, or can't keep
  up, its track is filled with silence, and it is reopened automatically when it
  comes back. Tracks always line up and end with equal length.
- Audio is captured to headerless raw files (`*.f32`) flushed to disk every
  second, so a crash loses at most about a second. Interrupted sessions are
  listed at startup and can be exported from there.
- The export is written alongside the raw file and finished with a rename; the
  raw file is deleted only once its export is safely on disk. If an export
  fails, the raw file is kept and exported again after Stop.
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
