# Desktop CLI Guide

The desktop CLI is the current hardware validation and capture tool for the Rust SDK. It scans for a WIZPR Ring, connects over BLE, records decoded 16 kHz mono PCM audio, and writes one session directory per run.

It is an example binary, not a product daemon or supported end-user app. Keep product API decisions in the SDK and keep experimental validation workflow here until the need is proven.

## Run

From the repository root:

```sh
cargo run --bin wizpr-ring-desktop -- list
cargo run --bin wizpr-ring-desktop -- listen --output-dir ./captures/macos
```

Useful options:

```sh
cargo run --bin wizpr-ring-desktop -- listen \
  --device-id <id-from-list> \
  --output-dir ./captures/macos \
  --log-audio-chunks
```

| Option | Purpose |
|---|---|
| `--device-id` | Connects to a specific device id or address shown by `list`. If omitted, `listen` connects to the first discovered ring. |
| `--output-dir` | Parent directory for generated session directories. |
| `--wav-gain` | Gain applied only when writing WAV files. Defaults to `3.0`; it does not change SDK audio. |
| `--log-audio-chunks` | Writes per-chunk diagnostics to `audio_chunks.jsonl`. Use for debugging, not routine captures. |

Advanced options are kept for transport debugging but hidden from normal help output:

| Hidden option | Purpose |
|---|---|
| `--scan-timeout` | Seconds to scan before failing. Defaults to `20`. |
| `--recording-drain-ms` | Extra time to collect audio after `RecordingStopped`. Defaults to `500`. |

## Console Output

Typical output:

```text
Initializing Bluetooth adapter...
Scanning for WIZPR Rings (up to 20s).... found 1.
Use `listen --device-id <id>` to connect to a specific candidate.
1. id=F7A4... name=WIZPR RING-A1:B2 address=AA:BB:CC:DD:EE:FF rssi=-58 wizpr_service=yes

Saving session to captures/macos/session-1779190054
Initializing Bluetooth adapter...
Scanning for WIZPR Ring (up to 20s)....... connected.
Audio and event receivers are ready.
-> operation event "SAMPLE_RATE: 16\r\n"
-> MicOn
-> recording started +++++++++++++++++++++++++
-> MicOff
-> recording stop received, draining audio for 500ms
-> recording saved captures/macos/session-1779190054/recording-001.wav (119 chunks, 53312 samples, 3.33s, gain 3.0x, 0 clipped)
```

Scan progress is shown with dots. Recording progress is shown with `+` characters. The CLI throttles progress flushing and buffers optional chunk logs so the diagnostic output does not unnecessarily backpressure the audio stream.

## Capture Layout

Each run creates a unique session directory:

```text
captures/
  macos/
    session-1779190054/
      events.jsonl
      operations.log
      audio_chunks.jsonl
      recording-001.wav
      recording-002.wav
```

Files:

| File | Description |
|---|---|
| `events.jsonl` | Structured lifecycle, recording, battery, and disconnect events. |
| `operations.log` | Unparsed operation events from the ring, for diagnostics only. |
| `audio_chunks.jsonl` | Optional per-audio-chunk diagnostics, created only with `--log-audio-chunks`. |
| `recording-*.wav` | 16 kHz mono PCM WAV files. |

## Audio Expectations

The SDK decodes IMA ADPCM into 16 kHz mono PCM. In recent macOS captures, one audio notification usually decoded to 448 PCM samples:

```text
448 samples / 16000 Hz = 28 ms
```

That is about 35.7 audio chunks per second. A five second recording should therefore contain roughly 178 chunks, though BLE scheduling can make chunk timing uneven.

### Event order

A recording that starts from idle produces `MicOn`, then `RecordingStarted`, then `MicOff`, then `RecordingStopped`. A recording that passes through the ring's ready state is preceded by `MicPreOn`. The CLI logs it as `-> MicPreOn` and writes it to `events.jsonl` as `{"event":"event","name":"MicPreOn"}`. A ready state that does not lead to a recording ends with `MicOff`.

Short presses still produce one recording each. Back-to-back recordings should never be merged into one file or dropped.

### Start of a recording

The two ways a recording can start look different:

- **From idle:** the first ~22 ms holds a large start transient. That transient is part of the signal the ring sends, not a decoding error. Raw recordings keep it. `AudioProcessing::Clean` mutes the first 42 ms of each recording, which covers it.
- **After the ready state:** no start transient. The recording starts at the normal signal level.

Very short presses (under ~0.4 s) can yield only one or two chunks, about 56 ms of audio. That audio is mostly the start transient and can reach full scale.

### Measuring DC offset and peaks

Capture with `--wav-gain 1` so the WAV holds the SDK samples unchanged. With the default gain of 3.0, the offset and peaks are scaled and may clip. Then summarize the session:

```sh
cargo run --bin wizpr-ring-desktop -- listen --wav-gain 1 --output-dir ./captures/macos
python3 scripts/wav_stats.py ./captures/macos/session-<id>
```

`wav_stats.py` uses only the Python standard library. For each recording it prints:

| Column | Meaning |
|---|---|
| `dc>42ms` | Mean of all samples after the first 42 ms, the span `Clean` mutes. Shown as `-` when less than 0.5 s of audio remains after the skip. |
| `pk<22ms`, `pk22-42` | Peak absolute value in the first 22 ms and in 22–42 ms: the start transient. |
| `pk>42ms` | Peak absolute value after 42 ms. |
| `clip` | Number of samples at ±32767/−32768. |

Compare recordings by how they started. The DC offset that `Clean` removes depends on the signal level at the moment a recording starts, so include recordings that start while you are already speaking loudly.

The desktop CLI records `Raw` only, so these steps do not exercise `AudioProcessing::Clean`.

## Validation Checklist

Use the same basic flow on macOS and Windows:

1. Build and test the workspace.
2. Run `list` and confirm the target ring appears.
3. Run `listen` with a platform-specific output directory, optionally passing `--device-id`.
4. Confirm scan and connect complete.
5. Trigger `MicOn` / `MicOff`. Record a few recordings that start from idle, a few that pass through the ready state (`MicPreOn` first), and a few short presses.
6. Confirm a WAV file is created and playable.
7. Confirm `events.jsonl` contains `recording_saved`, and that the event order matches [Event order](#event-order) with one recording per press.
8. Run `scripts/wav_stats.py` on the session and note `dc>42ms`, the start peaks, and clipping per start type. Recordings from idle should show the start transient and recordings after the ready state should not ([Start of a recording](#start-of-a-recording)).
9. Turn off or disconnect the ring and confirm the CLI reports a disconnect reason.

Recommended commands:

```sh
cargo test --workspace --all-targets
cargo run --bin wizpr-ring-desktop -- list
cargo run --bin wizpr-ring-desktop -- listen --wav-gain 1 --output-dir ./captures/macos
python3 scripts/wav_stats.py ./captures/macos/session-<id>
```

On Windows PowerShell:

```powershell
cargo test --workspace --all-targets
cargo run --bin wizpr-ring-desktop -- list
cargo run --bin wizpr-ring-desktop -- listen --wav-gain 1 --output-dir .\captures\windows
python scripts\wav_stats.py .\captures\windows\session-<id>
```

## CI Artifacts

GitHub Actions builds release desktop binaries for Linux, macOS, and Windows on every push and pull request.

Artifact names:

| Platform | Artifact |
|---|---|
| Linux | `wizpr-ring-desktop-Linux-X64` |
| macOS | `wizpr-ring-desktop-macOS-ARM64` or `wizpr-ring-desktop-macOS-X64` |
| Windows | `wizpr-ring-desktop-Windows-X64` |

The hosted CI artifacts prove that the CLI builds and starts with `--help`. They do not prove real BLE behavior. Hardware validation still needs a physical machine, Bluetooth adapter, and WIZPR Ring.

## Current Boundaries

The desktop example intentionally does not implement:

- Automatic reconnect.
- Background daemon mode.
- A command/control socket.
- PVAD inference.
- Firmware-specific experimental commands.

Those should move into dedicated internal tooling once the workflow needs them.
