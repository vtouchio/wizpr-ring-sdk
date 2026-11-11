//! IMA ADPCM decoder for the WIZPR Ring audio stream.
//!
//! The ring transmits audio as IMA ADPCM-compressed data over BLE.
//! Each byte holds two 4-bit nibbles, producing two 16-bit PCM samples
//! at 16 kHz mono. State is carried between BLE packets so a long
//! recording decodes continuously.

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const STEP_ADJUST: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// Streaming codec state carried across BLE packets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodecState {
    predicted_sample: i32,
    step_index: i32,
}

impl CodecState {
    pub const fn new() -> Self {
        Self {
            predicted_sample: 0,
            step_index: 0,
        }
    }

    /// Current predicted PCM sample. Within an `i16` for [`decode`]; in an
    /// [`AudioProcessing::Clean`](crate::AudioProcessing::Clean) segment it is
    /// the unclamped accumulator and may exceed the `i16` range.
    pub const fn predicted_sample(&self) -> i32 {
        self.predicted_sample
    }

    /// Current IMA ADPCM step table index.
    pub const fn step_index(&self) -> i32 {
        self.step_index
    }
}

/// Decode an ADPCM packet into a fresh `Vec<i16>` of PCM samples.
///
/// The caller-supplied `state` is mutated in place so the next packet
/// continues from the same prediction.
pub fn decode(data: &[u8], state: &mut CodecState) -> Vec<i16> {
    let mut out = Vec::with_capacity(data.len() * 2);
    decode_into(data, state, &mut out);
    out
}

/// Decode an ADPCM packet, appending PCM samples to an existing buffer.
///
/// Use this on a hot path to reuse a single allocation across packets.
pub fn decode_into(data: &[u8], state: &mut CodecState, out: &mut Vec<i16>) {
    out.reserve(data.len() * 2);
    for &byte in data {
        let high = (byte >> 4) & 0x0f;
        let low = byte & 0x0f;
        out.push(decode_sample(high, state));
        out.push(decode_sample(low, state));
    }
}

fn decode_sample(nibble: u8, state: &mut CodecState) -> i16 {
    let sample = (state.predicted_sample + nibble_diff(nibble, state.step_index))
        .clamp(i16::MIN as i32, i16::MAX as i32);
    state.predicted_sample = sample;
    advance_step_index(nibble, state);
    sample as i16
}

/// Bound for the wide accumulator. Real audio never gets close; it only
/// keeps a corrupt stream from overflowing `i32`.
const WIDE_LIMIT: i32 = 1 << 24;

/// Decode one nibble without clamping the predictor to `i16`.
///
/// When the decoder is offset from the encoder by `d`, clamping at
/// ±32767 clips the decoder before the encoder and changes `d`. Keeping
/// the accumulator wide keeps `d` constant so a DC blocker can remove it.
pub(crate) fn decode_sample_wide(nibble: u8, state: &mut CodecState) -> i32 {
    let sample = (state.predicted_sample + nibble_diff(nibble, state.step_index))
        .clamp(-WIDE_LIMIT, WIDE_LIMIT);
    state.predicted_sample = sample;
    advance_step_index(nibble, state);
    sample
}

fn nibble_diff(nibble: u8, step_index: i32) -> i32 {
    let step = STEP_TABLE[step_index as usize];
    let mut diff = step >> 3;
    if nibble & 0x01 != 0 {
        diff += step >> 2;
    }
    if nibble & 0x02 != 0 {
        diff += step >> 1;
    }
    if nibble & 0x04 != 0 {
        diff += step;
    }
    if nibble & 0x08 != 0 {
        diff = -diff;
    }
    diff
}

fn advance_step_index(nibble: u8, state: &mut CodecState) {
    state.step_index = (state.step_index + STEP_ADJUST[(nibble & 0x07) as usize]).clamp(0, 88);
}

/// Reference IMA ADPCM encoder used only by tests to model the ring.
#[cfg(test)]
pub(crate) mod test_encoder {
    use super::{decode_sample, CodecState, STEP_TABLE};

    /// Encode one sample, returning the nibble. `state` tracks the
    /// encoder's own reconstruction, exactly as the ring keeps it.
    pub(crate) fn encode_sample(sample: i16, state: &mut CodecState) -> u8 {
        let step = STEP_TABLE[state.step_index as usize];
        let mut diff = i32::from(sample) - state.predicted_sample;
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 0x08;
            diff = -diff;
        }
        if diff >= step {
            nibble |= 0x04;
            diff -= step;
        }
        if diff >= step >> 1 {
            nibble |= 0x02;
            diff -= step >> 1;
        }
        if diff >= step >> 2 {
            nibble |= 0x01;
        }
        // The encoder reconstructs with the same arithmetic as the decoder.
        decode_sample(nibble, state);
        nibble
    }

    /// Encode samples into packed bytes (high nibble first), returning the
    /// bytes and the encoder's reconstruction of each sample.
    pub(crate) fn encode(samples: &[i16], state: &mut CodecState) -> (Vec<u8>, Vec<i16>) {
        assert!(samples.len() % 2 == 0, "encode an even number of samples");
        let mut bytes = Vec::with_capacity(samples.len() / 2);
        let mut recon = Vec::with_capacity(samples.len());
        for pair in samples.chunks_exact(2) {
            let high = encode_sample(pair[0], state);
            recon.push(state.predicted_sample() as i16);
            let low = encode_sample(pair[1], state);
            recon.push(state.predicted_sample() as i16);
            bytes.push((high << 4) | low);
        }
        (bytes, recon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_byte_yields_two_samples() {
        let mut st = CodecState::new();
        let out = decode(&[0x12, 0x34, 0x56], &mut st);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn decodes_standard_ima_adpcm_known_vector_high_nibble_first() {
        let mut st = CodecState::new();
        let out = decode(&[0x12, 0x34, 0x56, 0x78], &mut st);

        assert_eq!(out, vec![1, 4, 8, 15, 27, 47, 88, 82]);
        assert_eq!(st.predicted_sample(), 82);
        assert_eq!(st.step_index(), 19);
    }

    #[test]
    fn fresh_state_starts_at_zero() {
        let st = CodecState::new();
        assert_eq!(st.predicted_sample(), 0);
        assert_eq!(st.step_index(), 0);
    }

    #[test]
    fn state_advances_across_calls() {
        let mut st = CodecState::new();
        decode(&[0x12, 0x34], &mut st);
        let after_first = st;
        assert_ne!(after_first.step_index(), 0);

        decode(&[0x56, 0x78], &mut st);
        assert_ne!(st.step_index(), after_first.step_index());
    }

    #[test]
    fn step_index_stays_in_range() {
        let mut st = CodecState::new();
        for _ in 0..1000 {
            decode(&[0x77; 32], &mut st);
            assert!((0..=88).contains(&st.step_index()));
        }
    }

    #[test]
    fn samples_stay_in_i16_range() {
        let mut st = CodecState::new();
        for byte in 0u8..=255 {
            let out = decode(&[byte; 8], &mut st);
            for s in out {
                let _ = s;
            }
        }
    }

    #[test]
    fn test_encoder_round_trips_through_decoder() {
        let input: Vec<i16> = (0..2000)
            .map(|n| ((n as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let (bytes, recon) = test_encoder::encode(&input, &mut CodecState::new());
        let decoded = decode(&bytes, &mut CodecState::new());
        assert_eq!(decoded, recon);
        let max_err = input
            .iter()
            .zip(&decoded)
            .skip(200)
            .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
            .max()
            .unwrap();
        assert!(max_err < 600, "max_err = {max_err}");
    }

    #[test]
    fn decode_into_appends_without_clearing() {
        let mut st = CodecState::new();
        let mut buf = vec![1i16, 2, 3];
        decode_into(&[0x12], &mut st, &mut buf);
        assert_eq!(buf.len(), 5);
        assert_eq!(&buf[..3], &[1, 2, 3]);
    }
}
