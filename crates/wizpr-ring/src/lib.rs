//! Cross-platform SDK for the WIZPR Ring.
//!
//! Discovers and connects to a ring over BLE (via `btleplug`), decodes
//! the IMA ADPCM audio stream, and surfaces ring events on async channels.
//!
//! ```no_run
//! use wizpr_ring::RingScanner;
//!
//! # async fn run() -> wizpr_ring::Result<()> {
//! let scanner = RingScanner::new().await?;
//! let devices = scanner.scan(std::time::Duration::from_secs(10)).await?;
//! let device = devices.into_iter().next().ok_or(wizpr_ring::Error::RingNotFound)?;
//! let conn = device.connect().await?;
//!
//! let mut audio = conn.audio()?;
//! while let Some(chunk) = audio.recv().await {
//!     println!("{} PCM samples", chunk.pcm_samples.len());
//! }
//! # Ok(())
//! # }
//! ```

pub mod connection;
pub mod error;
pub mod scanner;

pub use connection::RingConnection;
pub use error::{Error, Result};
pub use scanner::{RingDevice, RingScanner, KnownRing};

pub use wizpr_ring_core as core;
pub use wizpr_ring_core::{AudioChunk, RingEvent};
