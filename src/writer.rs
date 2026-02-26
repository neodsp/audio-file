use std::path::Path;

use num::Float;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("could not encode audio")]
    Encode(#[from] hound::Error),
}

/// Sample format for writing audio
#[derive(Debug, Clone, Copy, Default)]
pub enum SampleFormat {
    /// 8-bit integer samples
    Int8,
    /// 16-bit integer samples
    #[default]
    Int16,
    /// 32-bit integer samples
    Int32,
    /// 32-bit float samples
    Float32,
}

/// Configuration for writing audio to WAV files
#[derive(Default)]
pub struct WriteConfig {
    /// Sample format to use when writing
    pub sample_format: SampleFormat,
}

/// Write interleaved audio samples to a WAV file
pub fn write<F: Float>(
    path: impl AsRef<Path>,
    samples: &[F],
    num_channels: u16,
    sample_rate: u32,
    config: WriteConfig,
) -> Result<(), WriteError> {
    let spec = hound::WavSpec {
        channels: num_channels,
        sample_rate,
        bits_per_sample: match config.sample_format {
            SampleFormat::Int8 => 8,
            SampleFormat::Int16 => 16,
            SampleFormat::Int32 => 32,
            SampleFormat::Float32 => 32,
        },
        sample_format: match config.sample_format {
            SampleFormat::Int8 | SampleFormat::Int16 | SampleFormat::Int32 => {
                hound::SampleFormat::Int
            }
            SampleFormat::Float32 => hound::SampleFormat::Float,
        },
    };

    let mut writer = hound::WavWriter::create(path.as_ref(), spec)?;

    match config.sample_format {
        SampleFormat::Int8 => {
            for &sample in samples {
                let sample_i8 = (sample.clamp(F::one().neg(), F::one())
                    * F::from(i8::MAX).unwrap_or(F::zero()))
                .to_i8()
                .unwrap_or(0);
                writer.write_sample(sample_i8)?;
            }
        }
        SampleFormat::Int16 => {
            for &sample in samples {
                let sample_i16 = (sample.clamp(F::one().neg(), F::one())
                    * F::from(i16::MAX).unwrap_or(F::zero()))
                .to_i16()
                .unwrap_or(0);
                writer.write_sample(sample_i16)?;
            }
        }
        SampleFormat::Int32 => {
            for &sample in samples {
                let sample_i32 = (sample.clamp(F::one().neg(), F::one())
                    * F::from(i32::MAX).unwrap_or(F::zero()))
                .to_i32()
                .unwrap_or(0);
                writer.write_sample(sample_i32)?;
            }
        }
        SampleFormat::Float32 => {
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
pub fn write_block<P: AsRef<Path>, F: Float + 'static>(
    path: P,
    audio_block: impl audio_blocks::AudioBlock<F>,
    sample_rate: u32,
    config: WriteConfig,
) -> Result<(), WriteError> {
    let block = audio_blocks::Interleaved::from_block(&audio_block);
    write(
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
    fn test_round_trip_i8() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        write(
            "tmp0.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int8,
            },
        )
        .unwrap();

        let audio2 = read::<f32>("tmp0.wav", ReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        // 8-bit PCM has low precision. Symphonia normalizes by dividing by 128
        // (not 127), so max representable value is 127/128 ≈ 0.992, giving a
        // worst-case error of ~0.016 near full scale.
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 2e-2
        );

        // Clean up temporary file
        std::fs::remove_file("tmp0.wav").expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_round_trip_i16() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        write(
            "tmp1.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int16,
            },
        )
        .unwrap();

        let audio2 = read::<f32>("tmp1.wav", ReadConfig::default()).unwrap();
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
    fn test_round_trip_i32() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        write(
            "tmp3.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        )
        .unwrap();

        let audio2 = read::<f32>("tmp3.wav", ReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-4
        );

        // Clean up temporary file
        std::fs::remove_file("tmp3.wav").expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_round_trip_f32() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        write(
            "tmp2.wav",
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Float32,
            },
        )
        .unwrap();

        let audio2 = read::<f32>("tmp2.wav", ReadConfig::default()).unwrap();
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
