# WizprRingSDK (Rust)

Cross-platform Rust SDK for the [WIZPR Ring](https://wizpr.io/) — BLE audio streaming, IMA ADPCM decoding, and ring events.

Built on [`btleplug`](https://github.com/deviceplug/btleplug), so the same SDK targets macOS, Linux, Windows, iOS, and Android from one codebase.

## Crates

| Crate | Purpose |
|---|---|
| [`wizpr-ring-core`](crates/wizpr-ring-core) | Pure-Rust core: IMA ADPCM codec, event parser, WAV writer, BLE UUIDs. No I/O, no BLE — portable to WASM / FFI. |
| [`wizpr-ring`](crates/wizpr-ring) | High-level SDK over `btleplug`: `RingScanner` + `RingConnection` with async audio/event channels. |
| [`examples/desktop`](examples/desktop) | Reference binary: scan, connect, log events, save recordings as WAV. |

## Scope

This SDK is intended for application integration with WIZPR Ring devices:

- scan for ring candidates and connect to a selected device
- receive decoded 16 kHz mono PCM audio
- receive ring events and safe status updates
- reuse protocol constants, the ADPCM decoder, and WAV utilities

## Quickstart

```rust
use std::time::Duration;
use wizpr_ring::{RingEvent, RingScanner};

#[tokio::main]
async fn main() -> wizpr_ring::Result<()> {
    let scanner = RingScanner::new().await?;
    let devices = scanner.scan(Duration::from_secs(20)).await?;
    let device = devices.into_iter().next().ok_or(wizpr_ring::Error::RingNotFound)?;
    let conn = device.connect().await?;

    let mut audio  = conn.audio()?;
    let mut events = conn.events()?;

    tokio::spawn(async move {
        while let Some(evt) = events.recv().await {
            if let RingEvent::Click = evt { println!("click!"); }
        }
    });

    while let Some(chunk) = audio.recv().await {
        // chunk.pcm_samples: Vec<i16> at 16 kHz mono
    }
    Ok(())
}
```

## Build & test

```sh
cargo build
cargo test --workspace --all-targets
cargo run --bin wizpr-ring-desktop -- listen --output-dir ./captures/macos
```

GitHub Actions also uploads native desktop CLI artifacts for Linux, macOS, and Windows on each CI run.

The desktop CLI writes one session directory per run:

```text
captures/
  session-1778830000/
    events.jsonl
    operations.log
    audio_chunks.jsonl   # only with --log-audio-chunks
    recording-001.wav
```

See [docs/desktop-cli.md](docs/desktop-cli.md) for desktop CLI options, capture logs, and platform validation workflow.

## Platform notes

- **macOS**: works out of the box; first run prompts for Bluetooth permission.
- **Linux**: requires BlueZ ≥ 5.50 over D-Bus.
- **Windows 10/11**: uses WinRT BLE APIs. Build/test is covered by CI; real BLE audio must be validated on hardware.
- **iOS / Android**: bindings TBD (`napi-rs`, UniFFI). The `core` crate already compiles on those targets.

## License

Apache-2.0 — see [LICENSE](LICENSE).

Apache-2.0 covers copyright and patent grants for this SDK. It does not grant rights to use VTouch or WIZPR names, logos, or product branding. See [TRADEMARKS.md](TRADEMARKS.md).

## Security

Please report security issues privately. See [SECURITY.md](SECURITY.md).
