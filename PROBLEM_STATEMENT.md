When this project is in a mature state, this file should be removed, or its contents reorganized into other files.

# Original statement

User is on Windows, and wants to record multiple audio streams from different devices, simultaneously.

# Language/framework

Rust. Audio via cpal (WASAPI on Windows; ASIO deferred past v1), UI via egui/eframe.

# Build system

Cargo. Windows builds from the devcontainer cross-compile with `x86_64-pc-windows-gnu` (mingw-w64).

# Build environment

Prioritize being able to build from here (devcontainer).

Secondary:

- Windows (non-cross-compile)
- macOS
- Linux (Ubuntu as reference)

# Target environment

Prioritize being able to run on Windows.

Secondary:

- macOS
- Linux (Ubuntu as reference)

# Non-functional goals

## Usability

UI/UX should be simple but present all necessary functionality (selecting audio devices, export tracks separately to directory as flac or wav). Volume meter/waveform is nice for live feedback, but not needed in version 1.

## Reliability

Reliability is king. User should be able to start the recording and not have to worry about if it will break. The app should degrade gracefully in as many scenarios as possible, such as:

- Audio device disconnects during recording
- Disconnected audio device reconnects later
- Audio device lags or stops sending data
- Audio devices do not all agree on sample rate (if this is even possible on Windows)
- Audio devices use different API (ASIO, WASAPI, others?)
- Audio devices have different number of channels
- 1 hour+ long recording sessions
- Lag when writing to disk
- App crashes (should be able to recover most of the data)
- UI is minimized

As an example, if one audio device fails, the others should continue to record.

## Performance

Low RAM usage will help with reliability.

# Testing / GitHub CI

Generally good development practice. Some overhead, but worth it for code quality.