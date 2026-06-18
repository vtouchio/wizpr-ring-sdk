//! Platform-agnostic core for the WIZPR Ring SDK.
//!
//! Contains the IMA ADPCM audio codec, ring event parser, PCM→WAV writer,
//! and BLE service/characteristic UUIDs. No I/O, no BLE — pure data
//! transforms that can be reused by every transport (btleplug, WASM, FFI).

pub mod audio;
pub mod codec;
pub mod constants;
pub mod events;

pub use audio::{write_wav, AudioChunk, SAMPLE_RATE_HZ};
pub use codec::{decode, decode_into, CodecState};
pub use constants::{transfer_status, WizprBle};
pub use events::{parse_operation, RingEvent};
