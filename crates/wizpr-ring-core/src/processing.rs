//! Segment-aware ADPCM decoding with optional output cleanup.
//!
//! The host decoder restarts at every recording start, but the ring's
//! encoder state at that point is not always the reset state (for example
//! after the ring's ready state). The decoded audio then carries a
//! constant DC offset. [`AudioProcessing::Clean`] removes that
//! offset and keeps it from growing when peaks reach full scale.
//! [`AudioProcessing::Raw`] (the default) is the plain decoder.

use crate::codec::{self, CodecState};

/// How decoded audio is post-processed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioProcessing {
    /// Plain IMA ADPCM decode, identical to [`codec::decode`].
    #[default]
    Raw,
    /// Per-segment first-order DC blocker and a muted segment start.
    ///
    /// The predictor accumulates in `i32` without clamping, and samples
    /// are clamped to `i16` only after the DC blocker. The first
    /// [`CLEAN_MUTE_SAMPLES`] samples of each segment are output as zero
    /// to hide the ring's start transient and the blocker's settling.
    Clean,
}

/// DC blocker pole: `y[n] = x[n] − x[n−1] + 0.995·y[n−1]` (−3 dB ≈ 12.7 Hz at 16 kHz).
pub const CLEAN_DC_BLOCK_POLE: f32 = 0.995;

/// Samples muted at the start of each segment in [`AudioProcessing::Clean`] (42 ms at 16 kHz).
pub const CLEAN_MUTE_SAMPLES: usize = 672;

/// Streaming decoder that knows where recording segments start.
///
/// Call [`reset_segment`](Self::reset_segment) whenever the ring starts a
/// new encoder run (a recording start). A processing change made with
/// [`set_processing`](Self::set_processing) takes effect at the next
/// segment start, or immediately if the current segment has not decoded
/// any audio yet, so one segment never mixes raw and cleaned output.
#[derive(Debug, Clone, Default)]
pub struct AudioDecoder {
    requested: AudioProcessing,
    active: AudioProcessing,
    codec: CodecState,
    segment_samples: usize,
    dc_prev_in: f32,
    dc_prev_out: f32,
}

impl AudioDecoder {
    pub fn new(processing: AudioProcessing) -> Self {
        Self {
            requested: processing,
            active: processing,
            ..Self::default()
        }
    }

    /// Processing applied to the current segment.
    pub const fn processing(&self) -> AudioProcessing {
        self.active
    }

    /// Request a processing mode for the next segment.
    pub fn set_processing(&mut self, processing: AudioProcessing) {
        self.requested = processing;
        if self.segment_samples == 0 {
            self.active = processing;
        }
    }

    /// Codec state of the current segment.
    pub const fn codec_state(&self) -> CodecState {
        self.codec
    }

    /// Start a new segment: clear the ADPCM predictor and the cleanup state.
    pub fn reset_segment(&mut self) {
        self.active = self.requested;
        self.codec = CodecState::new();
        self.segment_samples = 0;
        self.dc_prev_in = 0.0;
        self.dc_prev_out = 0.0;
    }

    /// Decode one ADPCM packet into PCM samples.
    pub fn decode(&mut self, data: &[u8]) -> Vec<i16> {
        let mut out = Vec::with_capacity(data.len() * 2);
        match self.active {
            AudioProcessing::Raw => {
                codec::decode_into(data, &mut self.codec, &mut out);
                self.segment_samples = self.segment_samples.saturating_add(out.len());
            }
            AudioProcessing::Clean => {
                for &byte in data {
                    for nibble in [(byte >> 4) & 0x0f, byte & 0x0f] {
                        let wide = codec::decode_sample_wide(nibble, &mut self.codec);
                        out.push(self.clean_sample(wide));
                    }
                }
            }
        }
        out
    }

    fn clean_sample(&mut self, wide: i32) -> i16 {
        let x = wide as f32;
        let y = x - self.dc_prev_in + CLEAN_DC_BLOCK_POLE * self.dc_prev_out;
        self.dc_prev_in = x;
        self.dc_prev_out = y;

        let index = self.segment_samples;
        self.segment_samples = self.segment_samples.saturating_add(1);
        if index < CLEAN_MUTE_SAMPLES {
            return 0;
        }
        y.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::test_encoder;

    const PACKET_BYTES: usize = 112;

    fn decode_packets(decoder: &mut AudioDecoder, bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks(PACKET_BYTES)
            .flat_map(|packet| decoder.decode(packet))
            .collect()
    }

    /// Deterministic pseudo-random bytes (xorshift), so tests need no deps.
    fn noise_bytes(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 24) as u8
            })
            .collect()
    }

    /// Reference DC blocker in f64 for the "perfectly synced" baseline.
    fn dc_block(samples: &[i16]) -> Vec<f64> {
        let (mut prev_in, mut prev_out) = (0.0f64, 0.0f64);
        samples
            .iter()
            .map(|&s| {
                let x = f64::from(s);
                let y = x - prev_in + f64::from(CLEAN_DC_BLOCK_POLE) * prev_out;
                prev_in = x;
                prev_out = y;
                y
            })
            .collect()
    }

    fn mean(samples: impl Iterator<Item = f64>) -> f64 {
        let (sum, n) = samples.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
        sum / n as f64
    }

    /// Recording whose encoder state is not the reset state: the encoder
    /// first runs on `ready` audio that the decoder never sees, then the
    /// host decoder starts fresh. Returns the transmitted bytes and the encoder's own
    /// reconstruction of the recording, which is what a synced decoder
    /// would output.
    fn recording_after_ready(ready: &[i16], recording: &[i16]) -> (Vec<u8>, Vec<i16>) {
        let mut encoder = CodecState::new();
        let _unseen = test_encoder::encode(ready, &mut encoder);
        test_encoder::encode(recording, &mut encoder)
    }

    /// The unseen audio settles the encoder at a DC level; the recording has a
    /// quiet lead-in followed by near-full-scale tone bursts.
    fn ready_scenario() -> (Vec<i16>, Vec<i16>, std::ops::Range<usize>, usize) {
        let ready = vec![-2000i16; 4000];
        let quiet = 2000..6000;
        let loud_start = 6000;
        let recording: Vec<i16> = (0..16000)
            .map(|n| {
                if n < loud_start {
                    0
                } else {
                    let t = (n - loud_start) as f32;
                    ((t * 2.0 * std::f32::consts::PI * 200.0 / 16000.0).sin() * 31500.0) as i16
                }
            })
            .collect();
        (ready, recording, quiet, loud_start)
    }

    #[test]
    fn default_is_raw() {
        assert_eq!(AudioProcessing::default(), AudioProcessing::Raw);
        assert_eq!(AudioDecoder::default().processing(), AudioProcessing::Raw);
    }

    #[test]
    fn raw_matches_plain_decoder_byte_for_byte() {
        let bytes = noise_bytes(PACKET_BYTES * 300, 0x1234_5678);
        let mut decoder = AudioDecoder::default();
        let mut reference_state = CodecState::new();
        let mut got = Vec::new();
        let mut expected = Vec::new();

        for (i, packet) in bytes.chunks(PACKET_BYTES).enumerate() {
            if i % 97 == 0 {
                decoder.reset_segment();
                reference_state = CodecState::new();
            }
            got.extend(decoder.decode(packet));
            expected.extend(codec::decode(packet, &mut reference_state));
        }

        let got_bytes: Vec<u8> = got.iter().flat_map(|s| s.to_le_bytes()).collect();
        let expected_bytes: Vec<u8> = expected.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(got_bytes, expected_bytes);
    }

    #[test]
    fn ready_scenario_reproduces_offset_and_raw_clip_growth() {
        let (ready, recording, quiet, _) = ready_scenario();
        let (bytes, synced) = recording_after_ready(&ready, &recording);
        let raw = decode_packets(&mut AudioDecoder::default(), &bytes);

        let offset = |range: std::ops::Range<usize>| {
            mean(range.map(|n| f64::from(raw[n]) - f64::from(synced[n])))
        };
        let before = offset(quiet);
        let after = offset(recording.len() - 4000..recording.len());

        // The fresh decoder sits at a constant offset from the encoder ...
        assert!(before.abs() > 1000.0, "offset before peaks = {before}");
        // ... and clipping at ±32767 changes it once peaks arrive.
        assert!(
            (after - before).abs() > 300.0,
            "offset before = {before}, after = {after}"
        );
    }

    #[test]
    fn clean_removes_residual_dc_after_ready() {
        let (ready, recording, quiet, _) = ready_scenario();
        let (bytes, _) = recording_after_ready(&ready, &recording);
        let clean = decode_packets(&mut AudioDecoder::new(AudioProcessing::Clean), &bytes);

        assert!(clean[..CLEAN_MUTE_SAMPLES].iter().all(|&s| s == 0));
        let residual = mean(quiet.map(|n| f64::from(clean[n])));
        assert!(residual.abs() < 2.0, "residual DC = {residual}");
    }

    #[test]
    fn clean_offset_does_not_grow_when_peaks_hit_full_scale() {
        let (ready, recording, _, loud_start) = ready_scenario();
        let (bytes, synced) = recording_after_ready(&ready, &recording);
        let clean = decode_packets(&mut AudioDecoder::new(AudioProcessing::Clean), &bytes);
        let ideal = dc_block(&synced);

        // After the DC blocker settles, cleaned output tracks the
        // DC-blocked synced signal through the full-scale bursts.
        let max_err = (loud_start - 2000..recording.len())
            .map(|n| (f64::from(clean[n]) - ideal[n].round().clamp(-32768.0, 32767.0)).abs())
            .fold(0.0, f64::max);
        assert!(max_err <= 4.0, "max error vs synced = {max_err}");
    }

    #[test]
    fn processing_change_waits_for_next_segment() {
        let bytes = noise_bytes(PACKET_BYTES * 2, 7);
        let mut decoder = AudioDecoder::default();
        decoder.decode(&bytes[..PACKET_BYTES]);

        decoder.set_processing(AudioProcessing::Clean);
        assert_eq!(decoder.processing(), AudioProcessing::Raw);

        decoder.reset_segment();
        assert_eq!(decoder.processing(), AudioProcessing::Clean);
        assert!(decoder
            .decode(&bytes[PACKET_BYTES..])
            .iter()
            .all(|&s| s == 0));
    }

    #[test]
    fn processing_change_applies_now_before_any_audio() {
        let mut decoder = AudioDecoder::default();
        decoder.set_processing(AudioProcessing::Clean);
        assert_eq!(decoder.processing(), AudioProcessing::Clean);
    }

    #[test]
    fn reset_segment_clears_codec_and_mute_counter() {
        let mut decoder = AudioDecoder::new(AudioProcessing::Clean);
        decoder.decode(&[0x77; CLEAN_MUTE_SAMPLES]);
        assert_ne!(decoder.codec_state().step_index(), 0);

        decoder.reset_segment();
        assert_eq!(decoder.codec_state().step_index(), 0);
        assert_eq!(decoder.codec_state().predicted_sample(), 0);
        assert!(decoder.decode(&[0x77; 4]).iter().all(|&s| s == 0));
    }
}
