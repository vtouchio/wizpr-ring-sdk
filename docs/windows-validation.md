# Windows Validation Guide

This guide describes how to validate the Rust SDK and desktop CLI on a physical Windows machine.

GitHub Actions validates Windows build and unit tests, but hosted CI does not validate real Bluetooth behavior. Scan, connect, audio, and disconnect must be tested on hardware.

## Requirements

- Windows 10 or Windows 11.
- Bluetooth adapter enabled in Windows Settings.
- WIZPR Ring nearby and available for connection.
- Rust stable toolchain installed through `rustup`.
- Git installed.

Optional but useful:

- Windows Terminal or PowerShell 7.
- A second terminal window for checking generated WAV files.

## CI-Level Validation

The repository CI runs this level automatically on `windows-latest`:

```powershell
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace --all-targets
cargo run --bin wizpr-ring-desktop -- --help
```

This proves that:

- The workspace compiles on Windows.
- Unit tests and doctests pass.
- The `btleplug` Windows backend links successfully.
- The desktop example builds.

This does not prove that:

- A Windows Bluetooth adapter can scan for the ring.
- The ring can connect and discover services.
- Notifications are delivered.
- Audio can be captured from a real ring.

## Hardware Validation

Run these commands from the repository root on the Windows machine.

### 1. Build and Test

```powershell
cargo build --workspace --all-targets
cargo test --workspace --all-targets
cargo run --bin wizpr-ring-desktop -- --help
```

Expected result:

- Build succeeds.
- All unit tests pass.

### 2. Run the Desktop Listener

```powershell
cargo run --bin wizpr-ring-desktop -- listen --wav-gain 1 --output-dir .\captures\windows
```

`--wav-gain 1` writes the SDK samples unchanged, so the audio checks in step 4 measure real values. The default gain of 3.0 is easier to listen to, but it scales the offset and peaks and can clip.

Expected startup output:

```text
Saving session to .\captures\windows\session-...
Initializing Bluetooth adapter...
Scanning for WIZPR Ring (up to 20s)....... connected.
Audio and event receivers are ready.
```

If adapter initialization fails:

- Confirm Bluetooth is enabled in Windows Settings.
- Confirm the machine has a BLE-capable adapter.
- Restart Bluetooth from Windows Settings and retry.
- Reboot if the adapter is present but unavailable to applications.

### 3. Validate Events

Trigger ring actions that should produce events.

Expected examples:

```text
-> MicOn
-> recording started +++++++++++++++++++++++++
-> MicOff
-> recording stop received, draining audio for 500ms
-> recording saved .\captures\windows\session-...\recording-001.wav (119 chunks, 53312 samples, 3.33s, gain 3.0x, 0 clipped)
```

Operation event examples may also appear:

```text
-> operation event "SAMPLE_RATE: 16\r\n"
```

Record any unexpected operation event strings. They may indicate documented operation events that `wizpr-ring-core` does not yet classify.

Exercise the three ways a recording can start, a few times each:

- From idle: `MicOn` with no `MicPreOn` before it.
- After the ready state: `-> MicPreOn`, then `MicOn`.
- Short presses of under ~0.4 s.

Confirm that each press produces exactly one `recording saved` line and that no recording is merged or dropped. The expected order is in [Event order](desktop-cli.md#event-order).

### 4. Validate WAV Capture

After a recording stops, confirm that a WAV file was created:

```powershell
Get-ChildItem .\captures\windows\session-*\recording-*.wav
Get-ChildItem .\captures\windows\session-*\events.jsonl
Get-ChildItem .\captures\windows\session-*\operations.log
```

Expected result:

- One WAV file per recording.
- `events.jsonl` exists and contains structured event lines.
- `operations.log` exists and contains operation event lines when the ring emits unparsed operation events.
- File size grows with recording duration.
- The file can be opened by a standard audio player or inspection tool.

Summarize the audio of the session (Python 3, standard library only):

```powershell
python scripts\wav_stats.py .\captures\windows\session-<id>
```

For each start type, note `dc>42ms` (mean after the first 42 ms), the start peaks `pk<22ms` and `pk22-42`, and `clip`. Recordings from idle should show a large start transient in the first ~22 ms, and recordings after the ready state should not. See [Start of a recording](desktop-cli.md#start-of-a-recording).

For packet-level diagnostics, run with `--log-audio-chunks`:

```powershell
cargo run --bin wizpr-ring-desktop -- listen --output-dir .\captures\windows --log-audio-chunks
```

This creates `audio_chunks.jsonl`. Use it only while debugging because it writes one line per decoded audio chunk.

### 5. Validate Disconnect

Stop the process with `Ctrl+C`, or turn off / move away the ring.

Expected result:

- The process exits or reports disconnection without panic.
- No partially written WAV file should be produced after a clean `RecordingStopped` event.

### 6. Validate Reconnect Behavior

The current desktop example does not implement automatic reconnect. For now, validate reconnect manually:

```powershell
cargo run --bin wizpr-ring-desktop -- listen --output-dir .\captures\windows
```

Then:

1. Connect successfully.
2. Stop the process.
3. Start the command again.
4. Confirm the ring can be discovered and connected again.

Automatic reconnect should be tested later in dedicated validation tooling.

## Results Template

Use this template when recording a Windows validation run.

```text
Date:
Tester:
Windows version:
Device model:
Bluetooth adapter:
Rust version:
SDK commit:
Ring firmware:

Build/test:
Adapter init:
Scan:
Connect:
Events:
Event order (idle / ready / short press):
Audio/WAV:
wav_stats (per start type: dc>42ms, pk<22ms, pk22-42, clip):
Disconnect:
Manual reconnect:
Long run duration:

Observed operation events:
Observed failures:
Notes:
```

## Pass Criteria

Windows hardware validation is considered passing when:

- `cargo build --workspace --all-targets` passes.
- `cargo test --workspace --all-targets` passes.
- The desktop example initializes the Bluetooth adapter.
- The ring is discovered within the scan timeout.
- Service and characteristic discovery succeeds.
- Event notifications are received.
- At least one recording is saved as a playable WAV file.
- A multi-second spoken phrase produces a multi-second WAV duration, not only the initial start sound.
- A second run can connect again after the first process exits.

Reconnect stress, background operation, and PVAD are out of scope for the current desktop example. They belong in dedicated validation tooling.
