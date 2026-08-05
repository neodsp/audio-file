use std::fs::File;
use std::path::Path;

use num::Float;
use thiserror::Error;

use crate::wav;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteError {
    #[error("could not write file")]
    Io(#[from] std::io::Error),

    #[error("channel count must not be zero")]
    ZeroChannels,

    #[error("sample count ({samples}) is not a multiple of the channel count ({channels})")]
    UnalignedSamples { samples: usize, channels: u16 },

    #[error("file size ({bytes} bytes) exceeds the 4 GiB limit of the wav format")]
    FileTooLarge { bytes: u64 },

    #[error("frame size ({bytes} bytes) exceeds the 65535 byte limit of the wav format")]
    FrameTooLarge { bytes: u32 },

    #[error("byte rate ({bytes_per_second} bytes/s) exceeds the limit of the wav format")]
    ByteRateTooHigh { bytes_per_second: u64 },
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
    if num_channels == 0 {
        return Err(WriteError::ZeroChannels);
    }
    if !samples.len().is_multiple_of(num_channels as usize) {
        return Err(WriteError::UnalignedSamples {
            samples: samples.len(),
            channels: num_channels,
        });
    }

    // Resolve the byte layout before touching the filesystem, so that input the
    // wav format cannot describe does not leave a truncated file behind.
    let layout = wav::Layout::new(
        samples.len(),
        num_channels,
        sample_rate,
        config.sample_format,
    )?;

    // The encoder writes the header in one call and the samples in large
    // blocks, so there is nothing left for a BufWriter to coalesce. Writing
    // straight to the file also means every error surfaces here rather than
    // being discovered while a buffer is flushed on drop.
    let mut file = File::create(path.as_ref())?;
    wav::write(&mut file, &layout, samples)
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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
    fn test_invalid_input_is_rejected() {
        use super::*;

        match write::<f32>("tmp_invalid.wav", &[0.0], 0, 48000, WriteConfig::default()) {
            Err(WriteError::ZeroChannels) => (),
            other => panic!("{other:?}"),
        }

        match write::<f32>(
            "tmp_invalid.wav",
            &[0.0, 0.0, 0.0],
            2,
            48000,
            WriteConfig::default(),
        ) {
            Err(WriteError::UnalignedSamples {
                samples: 3,
                channels: 2,
            }) => (),
            other => panic!("{other:?}"),
        }

        // Both are rejected before the file is created
        assert!(!std::path::Path::new("tmp_invalid.wav").exists());
    }

    /// Input the wav format cannot describe is rejected up front too, so no
    /// half written file is left on disk.
    #[test]
    fn test_unrepresentable_input_leaves_no_file() {
        use super::*;

        let path = "tmp_unrepresentable.wav";
        match write::<f32>(
            path,
            &[],
            u16::MAX,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        ) {
            Err(WriteError::FrameTooLarge { .. }) => (),
            other => panic!("{other:?}"),
        }

        assert!(!std::path::Path::new(path).exists());
    }

    /// The reason this crate encodes wav itself. A mono float file must not be
    /// tagged `WAVEFORMATEXTENSIBLE`: its `dwChannelMask` can only name a
    /// physical speaker, and naming `SPEAKER_FRONT_LEFT` makes players route
    /// the audio to the left speaker alone.
    #[test]
    fn test_mono_files_are_not_speaker_assigned() {
        use super::*;

        let path = "tmp_mono_mask.wav";
        for sample_format in [
            SampleFormat::Int8,
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(path, &[0.0f32; 8], 1, 48000, WriteConfig { sample_format }).unwrap();
            let bytes = std::fs::read(path).unwrap();

            // wFormatTag sits at the start of the fmt chunk body.
            let tag = u16::from_le_bytes(bytes[20..22].try_into().unwrap());
            assert_ne!(tag, 0xfffe, "{sample_format:?} is extensible");
        }

        std::fs::remove_file(path).unwrap();
    }

    /// Multichannel output takes the extensible path, so check a decoder can
    /// still read it back.
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_round_trip_multichannel() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_4ch.wav", ReadConfig::default()).unwrap();
        assert_eq!(audio1.num_channels, 4);

        let path = "tmp_4ch.wav";
        for sample_format in [
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(
                path,
                &audio1.samples_interleaved,
                audio1.num_channels,
                audio1.sample_rate,
                WriteConfig { sample_format },
            )
            .unwrap();

            let audio2 = read::<f32>(path, ReadConfig::default()).unwrap();
            assert_eq!(audio2.num_channels, 4, "{sample_format:?}");
            assert_eq!(audio1.sample_rate, audio2.sample_rate);
            approx::assert_abs_diff_eq!(
                audio1.samples_interleaved.as_slice(),
                audio2.samples_interleaved.as_slice(),
                epsilon = 1e-4
            );
        }

        std::fs::remove_file(path).unwrap();
    }

    /// The odd frame counts that need a pad byte have to survive a round trip,
    /// since a decoder that trusts the chunk size would otherwise read into it.
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_round_trip_odd_length() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = "tmp_odd.wav";
        for num_frames in 0..6 {
            let samples: Vec<f32> = (0..num_frames).map(|i| i as f32 / 10.0).collect();

            write(
                path,
                &samples,
                1,
                48000,
                WriteConfig {
                    sample_format: SampleFormat::Int8,
                },
            )
            .unwrap();

            let audio = read::<f32>(path, ReadConfig::default()).unwrap();
            assert_eq!(
                audio.samples_interleaved.len(),
                num_frames,
                "{num_frames} frames"
            );
        }

        std::fs::remove_file(path).unwrap();
    }

    /// The public API is generic over the float type, so f64 input has to work
    /// end to end and not just in the encoder.
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_round_trip_f64_input() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = "tmp_f64.wav";
        let samples: Vec<f64> = (0..64).map(|i| (i as f64 / 32.0) - 1.0).collect();

        write(
            path,
            &samples,
            2,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Float32,
            },
        )
        .unwrap();

        let audio = read::<f64>(path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, 2);
        approx::assert_abs_diff_eq!(
            samples.as_slice(),
            audio.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(path).unwrap();
    }

    /// Full scale samples must not wrap around or drop out, which happens when
    /// the integer range is not exactly representable in the sample type.
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_full_scale_round_trip() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = "tmp_full_scale.wav";
        let samples = [1.0f32, -1.0, 0.5, -0.5, 0.0];

        for sample_format in [
            SampleFormat::Int8,
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(path, &samples, 1, 48000, WriteConfig { sample_format }).unwrap();
            let audio = read::<f32>(path, ReadConfig::default()).unwrap();

            approx::assert_abs_diff_eq!(
                samples.as_slice(),
                audio.samples_interleaved.as_slice(),
                epsilon = 1e-2
            );
        }

        std::fs::remove_file(path).unwrap();
    }

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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
