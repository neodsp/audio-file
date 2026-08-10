use std::fs::File;
use std::path::Path;

use num_traits::Float;
use thiserror::Error;

use crate::wav;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteError {
    #[error("could not write file")]
    Io(#[from] std::io::Error),

    #[error("channel count must not be zero")]
    ZeroChannels,

    #[error("sample rate must not be zero")]
    ZeroSampleRate,

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
    // A zero `nSamplesPerSec` does not describe a timeline, so the file would
    // not be playable and this crate's own wav decoder refuses to open it.
    // Anything that derives a frame position or a resampling ratio from the rate
    // divides by it, so the whole file is worth rejecting over.
    if sample_rate == 0 {
        return Err(WriteError::ZeroSampleRate);
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
    let path = path.as_ref();
    let mut file = File::create(path)?;
    let result = wav::write(&mut file, &layout, samples);
    // Close the file before the cleanup below, since Windows cannot remove a
    // path that is still open.
    drop(file);

    if let Err(err) = result {
        // A write that failed halfway leaves a truncated file behind, which must
        // not be mistaken for a finished one. Only a regular file is removed:
        // the path may be a device or a pipe that was not created here and has
        // to survive the failed write.
        if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
            let _ = std::fs::remove_file(path);
        }
        return Err(err);
    }

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

        let path = crate::tmp_path("round-trip-i8.wav");
        write(
            &path,
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int8,
            },
        )
        .unwrap();

        let audio2 = read::<f32>(&path, ReadConfig::default()).unwrap();
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
        std::fs::remove_file(&path).expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_round_trip_i16() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        let path = crate::tmp_path("round-trip-i16.wav");
        write(
            &path,
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int16,
            },
        )
        .unwrap();

        let audio2 = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-4
        );

        // Clean up temporary file
        std::fs::remove_file(&path).expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_round_trip_i32() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        let path = crate::tmp_path("round-trip-i32.wav");
        write(
            &path,
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        )
        .unwrap();

        let audio2 = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-4
        );

        // Clean up temporary file
        std::fs::remove_file(&path).expect("Failed to remove temporary test file");
    }

    #[test]
    fn test_invalid_input_is_rejected() {
        use super::*;

        let path = crate::tmp_path("invalid.wav");
        match write::<f32>(&path, &[0.0], 0, 48000, WriteConfig::default()) {
            Err(WriteError::ZeroChannels) => (),
            other => panic!("{other:?}"),
        }

        match write::<f32>(&path, &[0.0, 0.0, 0.0], 2, 48000, WriteConfig::default()) {
            Err(WriteError::UnalignedSamples {
                samples: 3,
                channels: 2,
            }) => (),
            other => panic!("{other:?}"),
        }

        // A file whose `nSamplesPerSec` is zero has no timeline, so it is not
        // playable and this crate's own decoder refuses to open it. Writing it
        // would produce a file that only the fallback decoder reads back, and
        // then with a sample rate of zero.
        match write::<f32>(&path, &[0.0, 0.0], 2, 0, WriteConfig::default()) {
            Err(WriteError::ZeroSampleRate) => (),
            other => panic!("{other:?}"),
        }

        // All three are rejected before the file is created
        assert!(!path.exists());
    }

    /// Input the wav format cannot describe is rejected up front too, so no
    /// half written file is left on disk.
    #[test]
    fn test_unrepresentable_input_leaves_no_file() {
        use super::*;

        let path = crate::tmp_path("unrepresentable.wav");
        match write::<f32>(
            &path,
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

        assert!(!path.exists());
    }

    /// The reason this crate encodes wav itself. A mono float file must not be
    /// tagged `WAVEFORMATEXTENSIBLE`: its `dwChannelMask` can only name a
    /// physical speaker, and naming `SPEAKER_FRONT_LEFT` makes players route
    /// the audio to the left speaker alone.
    #[test]
    fn test_mono_files_are_not_speaker_assigned() {
        use super::*;

        let path = crate::tmp_path("mono-mask.wav");
        for sample_format in [
            SampleFormat::Int8,
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(&path, &[0.0f32; 8], 1, 48000, WriteConfig { sample_format }).unwrap();
            let bytes = std::fs::read(&path).unwrap();

            // wFormatTag sits at the start of the fmt chunk body.
            let tag = u16::from_le_bytes(bytes[20..22].try_into().unwrap());
            assert_ne!(tag, 0xfffe, "{sample_format:?} is extensible");
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// Multichannel output takes the extensible path, so check a decoder can
    /// still read it back.
    #[test]
    fn test_round_trip_multichannel() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_4ch.wav", ReadConfig::default()).unwrap();
        assert_eq!(audio1.num_channels, 4);

        let path = crate::tmp_path("round-trip-4ch.wav");
        // Every sample format, since each pairs the extensible layout with a
        // different SubFormat GUID and sample width. 8-bit needs the loose
        // epsilon its precision allows.
        for (sample_format, epsilon) in [
            (SampleFormat::Int8, 2e-2),
            (SampleFormat::Int16, 1e-4),
            (SampleFormat::Int32, 1e-4),
            (SampleFormat::Float32, 1e-4),
        ] {
            write(
                &path,
                &audio1.samples_interleaved,
                audio1.num_channels,
                audio1.sample_rate,
                WriteConfig { sample_format },
            )
            .unwrap();

            let audio2 = read::<f32>(&path, ReadConfig::default()).unwrap();
            assert_eq!(audio2.num_channels, 4, "{sample_format:?}");
            assert_eq!(audio1.sample_rate, audio2.sample_rate);
            approx::assert_abs_diff_eq!(
                audio1.samples_interleaved.as_slice(),
                audio2.samples_interleaved.as_slice(),
                epsilon = epsilon
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// A channel count wide enough to need the extensible layout, round
    /// tripped through the native WAV decoder.
    #[test]
    fn test_round_trip_eighteen_channels() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("round-trip-18ch.wav");
        let num_channels = 18u16;
        let samples: Vec<f32> = (0..usize::from(num_channels) * 5)
            .map(|i| (i as f32 * 0.01) - 0.5)
            .collect();

        write(
            &path,
            &samples,
            num_channels,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        )
        .unwrap();

        let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, num_channels);
        approx::assert_abs_diff_eq!(
            samples.as_slice(),
            audio.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// Symphonia's WAV reader derives the channel layout from the extensible
    /// `dwChannelMask` and only knows 18 standard speaker positions, so it
    /// rejects a mask naming more than that. The native decoder in
    /// [`crate::wav`] reads `nChannels` directly and never looks at the mask,
    /// so a file with more channels than that ceiling now round trips too.
    #[test]
    fn test_many_channels_can_be_read_back() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("64ch.wav");
        let num_channels = 64u16;
        let total = usize::from(num_channels) * 5;
        let samples: Vec<f32> = (0..total)
            .map(|i| (i as f32 / total as f32) - 0.5)
            .collect();

        write(
            &path,
            &samples,
            num_channels,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        )
        .unwrap();

        let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, num_channels);
        approx::assert_abs_diff_eq!(
            samples.as_slice(),
            audio.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// A path that cannot be created has to surface as an error instead of a
    /// panic, since the encoder writes straight to the file.
    #[test]
    fn test_unwritable_path_is_reported() {
        use super::*;

        match write::<f32>(
            crate::tmp_path("missing-dir").join("nested/out.wav"),
            &[0.0; 4],
            1,
            48000,
            WriteConfig::default(),
        ) {
            Err(WriteError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
    }

    /// A write that fails halfway, here because the device is full, has to
    /// surface the error. `/dev/full` also covers the other half of the cleanup
    /// that follows such a failure: it removes a file it truncated, but not a
    /// path it did not create.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_failed_write_is_reported() {
        use super::*;

        match write::<f32>("/dev/full", &[0.0; 1024], 2, 48000, WriteConfig::default()) {
            Err(WriteError::Io(_)) => (),
            other => panic!("{other:?}"),
        }

        assert!(
            std::path::Path::new("/dev/full").exists(),
            "the failed write removed the device"
        );
    }

    /// `write_block` reinterleaves before handing the samples to the encoder, so
    /// a channel-major block has to come back in the same order it went in.
    #[cfg(feature = "audio-blocks")]
    #[test]
    fn test_round_trip_block() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("round-trip-block.wav");
        // Two channels of three frames, laid out channel after channel.
        let block = audio_blocks::Sequential::from_slice(&[0.1f32, 0.2, 0.3, -0.1, -0.2, -0.3], 2);

        write_block(
            &path,
            block,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Float32,
            },
        )
        .unwrap();

        let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, 2);
        assert_eq!(audio.sample_rate, 48000);
        approx::assert_abs_diff_eq!(
            [0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3].as_slice(),
            audio.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// The odd frame counts that need a pad byte have to survive a round trip,
    /// since a decoder that trusts the chunk size would otherwise read into it.
    #[test]
    fn test_round_trip_odd_length() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("round-trip-odd.wav");
        for num_frames in 0..6 {
            let samples: Vec<f32> = (0..num_frames).map(|i| i as f32 / 10.0).collect();

            write(
                &path,
                &samples,
                1,
                48000,
                WriteConfig {
                    sample_format: SampleFormat::Int8,
                },
            )
            .unwrap();

            let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
            assert_eq!(
                audio.samples_interleaved.len(),
                num_frames,
                "{num_frames} frames"
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// The public API is generic over the float type, so f64 input has to work
    /// end to end and not just in the encoder.
    #[test]
    fn test_round_trip_f64_input() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("round-trip-f64.wav");
        let samples: Vec<f64> = (0..64).map(|i| (i as f64 / 32.0) - 1.0).collect();

        write(
            &path,
            &samples,
            2,
            48000,
            WriteConfig {
                sample_format: SampleFormat::Float32,
            },
        )
        .unwrap();

        let audio = read::<f64>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, 2);
        approx::assert_abs_diff_eq!(
            samples.as_slice(),
            audio.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(&path).unwrap();
    }

    /// Full scale samples must not wrap around or drop out, which happens when
    /// the integer range is not exactly representable in the sample type.
    #[test]
    fn test_full_scale_round_trip() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let path = crate::tmp_path("full-scale.wav");
        let samples = [1.0f32, -1.0, 0.5, -0.5, 0.0];

        for sample_format in [
            SampleFormat::Int8,
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(&path, &samples, 1, 48000, WriteConfig { sample_format }).unwrap();
            let audio = read::<f32>(&path, ReadConfig::default()).unwrap();

            approx::assert_abs_diff_eq!(
                samples.as_slice(),
                audio.samples_interleaved.as_slice(),
                epsilon = 1e-2
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_round_trip_f32() {
        use super::*;
        use crate::reader::{ReadConfig, read};

        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();

        let path = crate::tmp_path("round-trip-f32.wav");
        write(
            &path,
            &audio1.samples_interleaved,
            audio1.num_channels,
            audio1.sample_rate,
            WriteConfig {
                sample_format: SampleFormat::Float32,
            },
        )
        .unwrap();

        let audio2 = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio1.sample_rate, audio2.sample_rate);
        approx::assert_abs_diff_eq!(
            audio1.samples_interleaved.as_slice(),
            audio2.samples_interleaved.as_slice(),
            epsilon = 1e-6
        );

        // Clean up temporary file
        std::fs::remove_file(&path).expect("Failed to remove temporary test file");
    }
}
