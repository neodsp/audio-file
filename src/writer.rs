use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};
use num::Float;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioWriteError {
    #[error("could not decode audio")]
    DecodingError(#[from] hound::Error),
}

/// Sample format for writing audio
#[derive(Debug, Clone, Copy, Default)]
pub enum WriteSampleFormat {
    /// 16-bit integer samples
    #[default]
    Int16,
    /// 32-bit float samples
    Float32,
}

/// Configuration for writing audio to WAV files
#[derive(Default)]
pub struct AudioWriteConfig {
    /// Sample format to use when writing
    pub sample_format: WriteSampleFormat,
}

/// Write interleaved audio samples to a WAV file
pub fn audio_write<F: Float>(
    path: impl AsRef<Path>,
    samples: &[F],
    num_channels: u16,
    sample_rate: u32,
    config: AudioWriteConfig,
) -> Result<(), AudioWriteError> {
    let spec = WavSpec {
        channels: num_channels,
        sample_rate,
        bits_per_sample: match config.sample_format {
            WriteSampleFormat::Int16 => 16,
            WriteSampleFormat::Float32 => 32,
        },
        sample_format: match config.sample_format {
            WriteSampleFormat::Int16 => SampleFormat::Int,
            WriteSampleFormat::Float32 => SampleFormat::Float,
        },
    };

    let mut writer = WavWriter::create(path.as_ref(), spec)?;

    match config.sample_format {
        WriteSampleFormat::Int16 => {
            // Convert samples to i16
            for &sample in samples {
                let sample_i16 = (sample.clamp(F::one().neg(), F::one())
                    * F::from(i16::MAX).unwrap_or(F::zero()))
                .to_i16()
                .unwrap_or(0);
                writer.write_sample(sample_i16)?;
            }
        }
        WriteSampleFormat::Float32 => {
            // Write f32 samples directly
            for &sample in samples {
                writer.write_sample(sample.to_f32().unwrap_or(0.0))?;
            }
        }
    }

    writer.finalize()?;

    Ok(())
}

/// Write audio from an AudioBlock to a WAV file
#[cfg(feature = "audio-blocks")]
pub fn audio_write_block<P: AsRef<Path>, F: Float + 'static>(
    path: P,
    audio_block: impl audio_blocks::AudioBlock<F>,
    sample_rate: u32,
    config: AudioWriteConfig,
) -> Result<(), AudioWriteError> {
    let block = audio_blocks::AudioBlockInterleaved::from_block(&audio_block);
    audio_write(
        path,
        block.raw_data(),
        audio_block.num_channels(),
        sample_rate,
        config,
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_round_trip_i16() {
        use super::*;
        use crate::reader::{AudioReadConfig, audio_read};

        let audio1 =
            audio_read::<f32>("test_data/test_1ch.wav", AudioReadConfig::default()).unwrap();

        audio_write(
            "tmp1.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            AudioWriteConfig {
                sample_format: WriteSampleFormat::Int16,
            },
        )
        .unwrap();

        let audio2 = audio_read::<f32>("tmp1.wav", AudioReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-4
        );

        // Clean up temporary file
        std::fs::remove_file("tmp1.wav").expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_round_trip_f32() {
        use super::*;
        use crate::reader::{AudioReadConfig, audio_read};

        let audio1 =
            audio_read::<f32>("test_data/test_1ch.wav", AudioReadConfig::default()).unwrap();

        audio_write(
            "tmp2.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            AudioWriteConfig {
                sample_format: WriteSampleFormat::Float32,
            },
        )
        .unwrap();

        let audio2 = audio_read::<f32>("tmp2.wav", AudioReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        // Clean up temporary file
        std::fs::remove_file("tmp2.wav").expect("Failed to remove temporary test file");
    }
}
