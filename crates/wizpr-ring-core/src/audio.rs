//! Audio chunk type and WAV writer.

use std::io::{self, Write};

/// Sample rate produced by the ring's IMA ADPCM stream.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// A chunk of decoded PCM audio from the ring.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// 16-bit signed PCM samples, mono.
    pub pcm_samples: Vec<i16>,
    /// Sample rate in Hz (always 16000 for the ring today).
    pub sample_rate: u32,
    /// Unix epoch millisecond timestamp when the chunk was received.
    pub timestamp_ms: u64,
}

/// Write a canonical 16-bit PCM mono WAV file into `writer`.
///
/// Returns the number of bytes written (header + samples). The writer
/// is generic so the same routine serializes to a file, a `Vec<u8>`,
/// or any other sink.
pub fn write_wav<W: Write>(writer: &mut W, samples: &[i16], sample_rate: u32) -> io::Result<usize> {
    let sizes = wav_sizes(samples.len())?;
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample) / 8;
    let block_align: u16 = num_channels * bits_per_sample / 8;

    writer.write_all(b"RIFF")?;
    writer.write_all(&sizes.riff_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;

    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?; // PCM
    writer.write_all(&num_channels.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&bits_per_sample.to_le_bytes())?;

    writer.write_all(b"data")?;
    writer.write_all(&sizes.data_size.to_le_bytes())?;
    for &s in samples {
        writer.write_all(&s.to_le_bytes())?;
    }

    Ok(sizes.total_size)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WavSizes {
    data_size: u32,
    riff_size: u32,
    total_size: usize,
}

fn wav_sizes(sample_count: usize) -> io::Result<WavSizes> {
    let data_size = sample_count
        .checked_mul(2)
        .ok_or_else(wav_too_large)?
        .try_into()
        .map_err(|_| wav_too_large())?;
    let riff_size = 36u32.checked_add(data_size).ok_or_else(wav_too_large)?;
    let total_size = 44usize
        .checked_add(sample_count.checked_mul(2).ok_or_else(wav_too_large)?)
        .ok_or_else(wav_too_large)?;

    Ok(WavSizes {
        data_size,
        riff_size,
        total_size,
    })
}

fn wav_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many samples for a canonical RIFF/WAV file",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_44_bytes_with_correct_magic() {
        let mut buf = Vec::new();
        let n = write_wav(&mut buf, &[0i16; 100], SAMPLE_RATE_HZ).unwrap();

        assert_eq!(n, 44 + 200);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        assert_eq!(&buf[36..40], b"data");
    }

    #[test]
    fn wav_header_encodes_sample_rate_little_endian() {
        let mut buf = Vec::new();
        write_wav(&mut buf, &[], 16_000).unwrap();
        assert_eq!(&buf[24..28], &16_000u32.to_le_bytes());
    }

    #[test]
    fn samples_are_written_little_endian_after_header() {
        let mut buf = Vec::new();
        write_wav(&mut buf, &[0x1234, -1], SAMPLE_RATE_HZ).unwrap();
        assert_eq!(&buf[44..46], &0x1234i16.to_le_bytes());
        assert_eq!(&buf[46..48], &(-1i16).to_le_bytes());
    }

    #[test]
    fn wav_size_rejects_riff_header_overflow() {
        let max_samples = ((u32::MAX - 36) / 2) as usize;
        let err = wav_sizes(max_samples + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn wav_size_accepts_largest_representable_riff() {
        let max_samples = ((u32::MAX - 36) / 2) as usize;
        let sizes = wav_sizes(max_samples).unwrap();

        assert_eq!(sizes.data_size, (max_samples * 2) as u32);
        assert_eq!(sizes.riff_size, u32::MAX - 1);
        assert_eq!(sizes.total_size, 44 + max_samples * 2);
    }
}
