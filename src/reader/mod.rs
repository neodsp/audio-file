//! Reading audio files.
//!
//! Two decoders share the work: the wav fast path right here in this module,
//! and `general`, the Symphonia-backed path for every other format, which
//! only exists when the `symphonia` feature is on. `decode` is where the two
//! meet - it tries the wav path first and only reaches for `general` when
//! that declines the file.

use std::fs::File;
use std::path::Path;

use num_traits::Float;
use thiserror::Error;

#[cfg(feature = "resample")]
use crate::resample::{ResampleError, resample};

#[cfg(feature = "symphonia")]
mod general;

/// What [`read`] needs of a sample type beyond being a float, which is whatever
/// the resampler needs of it. With the `resample` feature that is
/// `rubato::Sample`.
#[cfg(feature = "resample")]
pub use rubato::Sample as ResampleSample;

/// What [`read`] needs of a sample type beyond being a float. Without the
/// `resample` feature nothing is resampled, so this asks for nothing and every
/// type satisfies it. It exists so that the bound on [`read`] reads the same in
/// either build.
#[cfg(not(feature = "resample"))]
pub trait ResampleSample {}

#[cfg(not(feature = "resample"))]
impl<F> ResampleSample for F {}

/// Audio data with interleaved samples
#[derive(Debug, Clone)]
pub struct Audio<F> {
    /// Interleaved audio samples
    pub samples_interleaved: Vec<F>,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Number of channels
    pub num_channels: u16,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError {
    #[error("could not read file: {0}")]
    Io(#[from] std::io::Error),

    /// A packet the decoder rejected. The file is damaged, truncated, or in an
    /// encoding this build cannot decode. Nothing is skipped over: a file that
    /// cannot be decoded in full is not read at all.
    #[cfg(feature = "symphonia")]
    #[error("could not decode audio: {0}")]
    Decode(#[from] symphonia::core::errors::Error),

    /// No decoder in this build can read the file. Only reachable without the
    /// `symphonia` feature, where the built-in wav decoder is the whole reader
    /// and everything it does not recognize has nowhere left to go.
    #[error("no decoder in this build can read this file")]
    UnsupportedFormat,

    #[error("no track found")]
    NoTrack,

    #[error("no sample rate found")]
    NoSampleRate,

    #[error("could not determine the number of channels")]
    NoChannels,

    #[error("channel count ({0}) exceeds the supported maximum of 65535")]
    TooManyChannels(usize),

    #[error("start frame ({start}) must not exceed end frame ({end})")]
    InvalidFrameRange { start: usize, end: usize },

    #[error("start channel {start} out of bounds (file has {total} channels)")]
    InvalidStartChannel { start: usize, total: usize },

    #[error("channel count must not be zero")]
    ZeroChannels,

    #[error(
        "channel range out of bounds: {count} channels starting at channel {start} (file has {total} channels)"
    )]
    InvalidChannelRange {
        start: usize,
        count: usize,
        total: usize,
    },

    #[error("channel count changed mid-stream (was {expected}, now {found})")]
    ChannelCountChanged { expected: usize, found: usize },

    #[error("sample rate changed mid-stream (was {expected}, now {found})")]
    SampleRateChanged { expected: u32, found: u32 },

    /// Frames the stream never delivered, which the read cannot leave out
    /// without moving every later frame off the position it was asked for.
    #[cfg(feature = "symphonia")]
    #[error("frames {start}..{end} are missing, the file is damaged or incomplete")]
    MissingFrames { start: u64, end: u64 },

    #[cfg(feature = "resample")]
    #[error("resample failed")]
    Resample(#[from] ResampleError),
}

/// Position in the audio stream (for start or stop points)
#[derive(Default, Debug, Clone, Copy)]
pub enum Position {
    /// Start from beginning or read until the end (depending on context)
    #[default]
    Default,
    /// Specific time offset
    Time(std::time::Duration),
    /// Specific frame number (sample position across all channels)
    Frame(usize),
}

#[derive(Default)]
pub struct ReadConfig {
    /// Where to start reading audio (time or frame-based), inclusive
    pub start: Position,
    /// Where to stop reading audio (time or frame-based), exclusive
    pub stop: Position,
    /// Starting channel to extract (0-indexed). None means start from channel 0.
    pub start_channel: Option<usize>,
    /// Number of channels to extract. None means extract all remaining channels.
    pub num_channels: Option<usize>,
    /// If specified the audio will be resampled to the given sample rate.
    ///
    /// Only present with the `resample` feature, so that a build without it
    /// cannot ask for a rate that nothing would resample to.
    #[cfg(feature = "resample")]
    pub sample_rate: Option<u32>,
}

/// Upper bound for the pre-allocation derived from the container metadata, so
/// that a bogus frame count cannot request a huge allocation up front. The
/// buffer still grows beyond this if the file really is that long.
///
/// Both decoding paths respect it, so a hostile header costs the same either
/// way.
pub(crate) const MAX_PREALLOC_SAMPLES: usize = 16 * 1024 * 1024;

/// Read an audio file from disk.
///
/// Only the selected range is decoded and stored. `F` is the sample type of the
/// returned audio, either `f32` or `f64`, normalized to `[-1.0, 1.0]`.
///
/// The `stop` position of [`ReadConfig`] is exclusive, so reading from frame 100
/// to frame 200 yields 100 frames. A `start` position beyond the end of the file
/// yields no samples.
pub fn read<F: Float + ResampleSample>(
    path: impl AsRef<Path>,
    config: ReadConfig,
) -> Result<Audio<F>, ReadError> {
    let decoded = decode::<F>(path.as_ref(), &config)?;
    let num_channels = checked_num_channels(decoded.num_channels)?;
    let (samples, sample_rate) = resolve_output_rate(decoded, &config)?;

    Ok(Audio {
        samples_interleaved: samples,
        sample_rate,
        num_channels,
    })
}

/// Resample to the rate requested in `config`, if any and if it differs from
/// the decoded rate. Without the `resample` feature there is no rate to
/// resample to, so the decoded audio passes through unchanged.
#[cfg(feature = "resample")]
fn resolve_output_rate<F: Float + ResampleSample>(
    decoded: Decoded<F>,
    config: &ReadConfig,
) -> Result<(Vec<F>, u32), ReadError> {
    Ok(match config.sample_rate {
        Some(sr_out) if sr_out != decoded.sample_rate => (
            resample(
                &decoded.samples,
                decoded.num_channels,
                decoded.sample_rate,
                sr_out,
            )?,
            sr_out,
        ),
        _ => (decoded.samples, decoded.sample_rate),
    })
}

#[cfg(not(feature = "resample"))]
fn resolve_output_rate<F>(
    decoded: Decoded<F>,
    _config: &ReadConfig,
) -> Result<(Vec<F>, u32), ReadError> {
    Ok((decoded.samples, decoded.sample_rate))
}

/// The channel count is reported as a `u16`, so a stream with more channels than
/// that cannot be described by [`Audio`].
fn checked_num_channels(count: usize) -> Result<u16, ReadError> {
    u16::try_from(count).map_err(|_| ReadError::TooManyChannels(count))
}

/// Decoded audio at the sample rate of the file, before any resampling.
struct Decoded<F> {
    samples: Vec<F>,
    num_channels: usize,
    sample_rate: u32,
}

/// Channel layout of the decoded stream, resolved against the read config.
#[derive(Clone, Copy)]
struct Layout {
    /// Channels per frame in the file.
    ///
    /// Only the general decoder needs it, to stride across a decoded packet.
    /// The wav path indexes the selected channels straight out of the file.
    #[cfg_attr(not(feature = "symphonia"), allow(dead_code))]
    total: usize,
    /// First channel to extract
    start: usize,
    /// Number of channels to extract
    count: usize,
}

/// Everything about a read that can only be resolved once the audio
/// specification is known: the frame positions depend on the sample rate, and
/// the channel selection on the channel count.
///
/// It is resolved from the first packet that decodes, because container metadata
/// can contradict the bitstream headers. A Matroska `SamplingFrequency` element
/// may disagree with the FLAC stream info it wraps, symphonia's demuxers report
/// the container value, and only some decoders amend their codec parameters with
/// what they read from the bitstream. The specification of decoded audio is
/// therefore the only reliable source, and the declared one is used only for
/// files without a single decodable packet.
#[derive(Clone, Copy)]
struct Plan {
    sample_rate: u32,
    layout: Layout,
    /// First frame to copy, inclusive
    start_frame: usize,
    /// Frame to stop before, if the read is bounded
    end_frame: Option<usize>,
}

impl Plan {
    /// Resolve and validate the read config against an audio specification.
    fn resolve(sample_rate: u32, channels: usize, config: &ReadConfig) -> Result<Self, ReadError> {
        let start_frame = position_to_frame(config.start, sample_rate).unwrap_or(0);
        let end_frame = position_to_frame(config.stop, sample_rate);

        if let Some(end_frame) = end_frame
            && start_frame > end_frame
        {
            return Err(ReadError::InvalidFrameRange {
                start: start_frame,
                end: end_frame,
            });
        }

        let (start, count) = channel_range(config, channels)?;
        // `Audio` reports the channel count as a `u16`, so a selection it cannot
        // describe is rejected here instead of after the whole file is decoded.
        checked_num_channels(count)?;

        Ok(Self {
            sample_rate,
            layout: Layout {
                total: channels,
                start,
                count,
            },
            start_frame,
            end_frame,
        })
    }
}

/// Attempts the WAV fast path: parsing the header directly and reading only
/// the requested bytes. PCM audio in a WAV file is a flat byte array, so a
/// frame range and a channel range are read by indexing into it directly,
/// with none of the packet timestamps, decoder warm-up or seek verification
/// the general path below needs for compressed formats.
///
/// Returns `Ok(None)` for anything the native decoder does not handle - a
/// file that is not WAV, or a WAV sample encoding it does not decode, such
/// as ADPCM - so the caller falls back to the general path.
fn try_native_wav<F: Float>(
    path: &Path,
    config: &ReadConfig,
) -> Result<Option<Decoded<F>>, ReadError> {
    let file = File::open(path)?;
    let Some(wav) = crate::wav::open_wav(file)? else {
        return Ok(None);
    };

    let plan = Plan::resolve(wav.sample_rate, wav.num_channels, config)?;
    let samples = crate::wav::read_frames::<F>(
        wav,
        plan.start_frame,
        plan.end_frame,
        plan.layout.start,
        plan.layout.count,
    )?;

    Ok(Some(Decoded {
        samples,
        num_channels: plan.layout.count,
        sample_rate: plan.sample_rate,
    }))
}

fn decode<F: Float>(path: &Path, config: &ReadConfig) -> Result<Decoded<F>, ReadError> {
    if let Some(decoded) = try_native_wav(path, config)? {
        return Ok(decoded);
    }

    // Without Symphonia the wav fast path is the whole reader, so declining a
    // file is the end of the line rather than a handover.
    #[cfg(not(feature = "symphonia"))]
    {
        Err(ReadError::UnsupportedFormat)
    }
    #[cfg(feature = "symphonia")]
    {
        general::decode_with_symphonia(path, config)
    }
}

/// Resolve and validate the requested channel range against a file with `total` channels.
fn channel_range(config: &ReadConfig, total: usize) -> Result<(usize, usize), ReadError> {
    let start = config.start_channel.unwrap_or(0);
    if start >= total {
        return Err(ReadError::InvalidStartChannel { start, total });
    }

    let count = config.num_channels.unwrap_or(total - start);
    if count == 0 {
        return Err(ReadError::ZeroChannels);
    }
    // The end of the range is only needed for this comparison, and a requested
    // count close to `usize::MAX` would overflow while calculating it.
    if start.checked_add(count).is_none_or(|end| end > total) {
        return Err(ReadError::InvalidChannelRange {
            start,
            count,
            total,
        });
    }

    Ok((start, count))
}

fn position_to_frame(position: Position, sample_rate: u32) -> Option<usize> {
    match position {
        Position::Default => None,
        Position::Time(duration) => {
            Some((duration.as_secs_f64() * sample_rate as f64).round() as usize)
        }
        Position::Frame(frame) => Some(frame),
    }
}

#[cfg(feature = "audio-blocks")]
pub fn read_block<F: num_traits::Float + 'static + ResampleSample>(
    path: impl AsRef<Path>,
    config: ReadConfig,
) -> Result<(audio_blocks::Interleaved<F>, u32), ReadError> {
    let audio = read(path, config)?;
    Ok((
        audio_blocks::Interleaved::from_slice(&audio.samples_interleaved, audio.num_channels),
        audio.sample_rate,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reading a WAV fixture needs no feature at all: the built-in decoder
    // handles integer PCM and IEEE float in every build. Everything in this
    // module tests that path; the Symphonia-backed path has its own tests in
    // `general`.
    use audio_blocks::{AudioBlock, InterleavedView};
    use std::time::Duration;

    fn to_block<F: num_traits::Float + 'static>(audio: &Audio<F>) -> InterleavedView<'_, F> {
        InterleavedView::from_slice(&audio.samples_interleaved, audio.num_channels)
    }

    /// Without Symphonia the WAV decoder is the whole reader, so a file it does
    /// not recognize has nowhere left to go. That has to be said plainly rather
    /// than surfacing as a missing track or a decode failure.
    #[cfg(not(feature = "symphonia"))]
    #[test]
    fn test_unsupported_format_without_the_general_decoder() {
        match read::<f32>("test_data/test_mp3.mp3", ReadConfig::default()) {
            Err(ReadError::UnsupportedFormat) => (),
            other => panic!("{:?}", other.map(|audio| audio.num_channels)),
        }
    }

    /// The point of making Symphonia optional: writing a WAV file and reading it
    /// back is the whole job in a build without it, so it has to be covered
    /// there and not only in the configurations that pull Symphonia in.
    #[test]
    fn test_wav_round_trips_without_the_general_decoder() {
        use crate::writer::{SampleFormat, WriteConfig, write};

        let path = crate::tmp_path("no-symphonia-round-trip.wav");
        let samples: Vec<f32> = (0..96).map(|i| (i as f32 / 48.0) - 1.0).collect();

        for sample_format in [
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            write(&path, &samples, 3, 48_000, WriteConfig { sample_format }).unwrap();

            let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
            assert_eq!(audio.num_channels, 3, "{sample_format:?}");
            assert_eq!(audio.sample_rate, 48_000, "{sample_format:?}");
            approx::assert_abs_diff_eq!(
                samples.as_slice(),
                audio.samples_interleaved.as_slice(),
                epsilon = 1e-4
            );

            // And a frame plus channel selection out of the middle of it.
            let audio = read::<f32>(
                &path,
                ReadConfig {
                    start: Position::Frame(4),
                    stop: Position::Frame(9),
                    start_channel: Some(1),
                    num_channels: Some(2),
                    #[cfg(feature = "resample")]
                    sample_rate: None,
                },
            )
            .unwrap();
            assert_eq!(audio.num_channels, 2, "{sample_format:?}");
            let src = samples.as_slice();
            let expected: Vec<f32> = (4..9)
                .flat_map(|frame| (1..3).map(move |ch| src[frame * 3 + ch]))
                .collect();
            approx::assert_abs_diff_eq!(
                expected.as_slice(),
                audio.samples_interleaved.as_slice(),
                epsilon = 1e-4
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// Verify that the read audio data matches the expected sine wave values.
    /// The test file was generated by utils/generate_wav.py with these parameters:
    /// - 4 channels with frequencies: [440, 554.37, 659.25, 880] Hz
    /// - Sample rate: 48000 Hz
    /// - Duration: 1 second (48000 samples)
    #[test]
    fn test_sine_wave_data_integrity() {
        const SAMPLE_RATE: f64 = 48000.0;
        const N_SAMPLES: usize = 48000;
        const FREQUENCIES: [f64; 4] = [440.0, 554.37, 659.25, 880.0];

        let audio = read::<f32>("test_data/test_4ch.wav", ReadConfig::default()).unwrap();
        let block = to_block(&audio);

        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(block.num_frames(), N_SAMPLES);
        assert_eq!(block.num_channels(), 4);

        // Verify each channel contains the expected sine wave
        for (ch, &freq) in FREQUENCIES.iter().enumerate() {
            for frame in 0..N_SAMPLES {
                let expected =
                    (2.0 * std::f64::consts::PI * freq * frame as f64 / SAMPLE_RATE).sin() as f32;
                let actual = block.sample(ch as u16, frame);
                assert!(
                    (actual - expected).abs() < 1e-4,
                    "Mismatch at channel {ch}, frame {frame}: expected {expected}, got {actual}"
                );
            }
        }

        // Also verify reading with an offset works consistently
        let audio = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                start: Position::Frame(24000),
                stop: Position::Frame(24100),
                ..Default::default()
            },
        )
        .unwrap();
        let block = to_block(&audio);

        for (ch, &freq) in FREQUENCIES.iter().enumerate() {
            for frame in 0..100 {
                let actual_frame = 24000 + frame;
                let expected = (2.0 * std::f64::consts::PI * freq * actual_frame as f64
                    / SAMPLE_RATE)
                    .sin() as f32;
                let actual = block.sample(ch as u16, frame);
                assert!(
                    (actual - expected).abs() < 1e-4,
                    "Offset mismatch at channel {ch}, frame {actual_frame}: expected {expected}, got {actual}"
                );
            }
        }
    }

    #[test]
    fn test_samples_selection() {
        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 1);

        let audio2 = read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start: Position::Frame(1100),
                stop: Position::Frame(1200),
                ..Default::default()
            },
        )
        .unwrap();
        let block2 = to_block(&audio2);
        assert_eq!(audio2.sample_rate, 48000);
        assert_eq!(block2.num_frames(), 100);
        assert_eq!(block2.num_channels(), 1);
        assert_eq!(block1.raw_data()[1100..1200], block2.raw_data()[..]);
    }

    #[test]
    fn test_time_selection() {
        let audio1 = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 1);

        let audio2 = read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start: Position::Time(Duration::from_secs_f32(0.5)),
                stop: Position::Time(Duration::from_secs_f32(0.6)),
                ..Default::default()
            },
        )
        .unwrap();
        let block2 = to_block(&audio2);

        assert_eq!(audio2.sample_rate, 48000);
        assert_eq!(block2.num_frames(), 4800);
        assert_eq!(block2.num_channels(), 1);
        assert_eq!(block1.raw_data()[24000..28800], block2.raw_data()[..]);
    }

    #[test]
    fn test_channel_selection() {
        let audio1 = read::<f32>("test_data/test_4ch.wav", ReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 4);

        let audio2 = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                start_channel: Some(1),
                num_channels: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let block2 = to_block(&audio2);

        assert_eq!(audio2.sample_rate, 48000);
        assert_eq!(block2.num_frames(), 48000);
        assert_eq!(block2.num_channels(), 2);

        // Verify we extracted channels 1 and 2 (skipping channel 0 and 3)
        for frame in 0..10 {
            assert_eq!(block2.sample(0, frame), block1.sample(1, frame));
            assert_eq!(block2.sample(1, frame), block1.sample(2, frame));
        }
    }

    #[test]
    fn test_fail_selection() {
        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start: Position::Frame(100),
                stop: Position::Frame(99),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidFrameRange { start: _, end: _ }) => (),
            _ => panic!(),
        }

        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start: Position::Time(Duration::from_secs_f32(0.6)),
                stop: Position::Time(Duration::from_secs_f32(0.5)),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidFrameRange { start: _, end: _ }) => (),
            _ => panic!(),
        }

        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start_channel: Some(1),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidStartChannel { start: _, total: _ }) => (),
            _ => panic!(),
        }

        // A start channel beyond the channel count must not overflow while
        // defaulting the channel count to "all remaining channels"
        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start_channel: Some(3),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidStartChannel { start: 3, total: 1 }) => (),
            other => panic!("{other:?}"),
        }

        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                num_channels: Some(0),
                ..Default::default()
            },
        ) {
            Err(ReadError::ZeroChannels) => (),
            _ => panic!(),
        }

        match read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                num_channels: Some(2),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidChannelRange {
                start: 0,
                count: 2,
                total: 1,
            }) => (),
            other => panic!("{other:?}"),
        }

        // A channel count that overflows the end of the range must be reported
        // instead of overflowing while validating or formatting it
        let error = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                start_channel: Some(1),
                num_channels: Some(usize::MAX),
                ..Default::default()
            },
        )
        .expect_err("a channel count of usize::MAX must be rejected");

        assert!(
            matches!(
                error,
                ReadError::InvalidChannelRange {
                    start: 1,
                    count: usize::MAX,
                    total: 4,
                }
            ),
            "{error:?}"
        );
        assert!(!error.to_string().is_empty());
    }

    #[cfg(feature = "resample")]
    #[test]
    fn test_resample_preserves_frequency() {
        const FREQUENCIES: [f64; 4] = [440.0, 554.37, 659.25, 880.0];
        let sr_out: u32 = 22050;

        // Read and resample in one step
        let audio = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                sample_rate: Some(sr_out),
                ..Default::default()
            },
        )
        .unwrap();
        let block = to_block(&audio);

        assert_eq!(audio.sample_rate, sr_out); // Resampled sample rate is returned
        assert_eq!(block.num_channels(), 4);

        // Expected frames after resampling: 48000 * (22050/48000) = 22050
        let expected_frames = 22050;
        assert_eq!(
            block.num_frames(),
            expected_frames,
            "Expected {} frames, got {}",
            expected_frames,
            block.num_frames()
        );

        // Verify sine wave frequencies are preserved after resampling
        // Skip first ~100 samples to avoid any edge effects from resampling
        let start_frame = 100;
        let test_frames = 1000;

        for (ch, &freq) in FREQUENCIES.iter().enumerate() {
            let mut max_error: f32 = 0.0;
            for frame in start_frame..(start_frame + test_frames) {
                let expected =
                    (2.0 * std::f64::consts::PI * freq * frame as f64 / sr_out as f64).sin() as f32;
                let actual = block.sample(ch as u16, frame);
                let error = (actual - expected).abs();
                max_error = max_error.max(error);
            }
            assert!(
                max_error < 0.02,
                "Channel {} ({}Hz): max error {} exceeds threshold",
                ch,
                freq,
                max_error
            );
        }
    }

    #[cfg(feature = "resample")]
    #[test]
    fn test_channel_selection_with_resampling() {
        // This test verifies that channel selection combined with resampling works correctly
        const FREQUENCIES: [f64; 4] = [440.0, 554.37, 659.25, 880.0];
        let sr_out: u32 = 22050;

        // Read channels 1 and 2 (indices 1 and 2) with resampling
        let audio = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                start_channel: Some(1),
                num_channels: Some(2),
                sample_rate: Some(sr_out),
                ..Default::default()
            },
        )
        .unwrap();
        let block = to_block(&audio);

        assert_eq!(audio.num_channels, 2, "Should have 2 channels");
        assert_eq!(
            audio.sample_rate, sr_out,
            "Sample rate should be the resampled rate"
        );

        // Expected frames after resampling: 48000 * (22050/48000) = 22050
        let expected_frames = 22050;
        assert_eq!(
            block.num_frames(),
            expected_frames,
            "Expected {} frames, got {}",
            expected_frames,
            block.num_frames()
        );

        // Verify that the resampled audio contains the correct frequencies
        // Channels 1 and 2 should have frequencies 554.37 Hz and 659.25 Hz
        let selected_freqs = &FREQUENCIES[1..3];

        let start_frame = 100;
        let test_frames = 1000;

        for (ch, &freq) in selected_freqs.iter().enumerate() {
            let mut max_error: f32 = 0.0;
            for frame in start_frame..(start_frame + test_frames) {
                let expected =
                    (2.0 * std::f64::consts::PI * freq * frame as f64 / sr_out as f64).sin() as f32;
                let actual = block.sample(ch as u16, frame);
                let error = (actual - expected).abs();
                max_error = max_error.max(error);
            }
            assert!(
                max_error < 0.02,
                "Channel {} ({}Hz): max error {} exceeds threshold",
                ch,
                freq,
                max_error
            );
        }
    }

    #[test]
    fn test_channel_count_must_fit_the_reported_type() {
        assert_eq!(checked_num_channels(2).unwrap(), 2);
        assert_eq!(
            checked_num_channels(usize::from(u16::MAX)).unwrap(),
            u16::MAX
        );

        let error = checked_num_channels(usize::from(u16::MAX) + 1)
            .expect_err("more channels than u16 can hold must be rejected");
        assert!(
            matches!(error, ReadError::TooManyChannels(65_536)),
            "{error:?}"
        );
        assert!(!error.to_string().is_empty());

        // The same limit is enforced when the read plan is resolved, before any
        // decoding happens
        assert!(matches!(
            Plan::resolve(48_000, 65_536, &ReadConfig::default()),
            Err(ReadError::TooManyChannels(65_536))
        ));
        // ... while selecting fewer channels than the file has stays allowed
        let plan = Plan::resolve(
            48_000,
            65_536,
            &ReadConfig {
                num_channels: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(plan.layout.count, 2);
    }

    #[test]
    fn test_plan_is_resolved_against_the_given_specification() {
        let config = ReadConfig {
            start: Position::Time(std::time::Duration::from_millis(10)),
            stop: Position::Time(std::time::Duration::from_millis(20)),
            start_channel: Some(1),
            num_channels: Some(2),
            #[cfg(feature = "resample")]
            sample_rate: None,
        };

        // Frame positions follow the given sample rate, not the config
        let plan = Plan::resolve(48_000, 4, &config).unwrap();
        assert_eq!(plan.sample_rate, 48_000);
        assert_eq!(plan.start_frame, 480);
        assert_eq!(plan.end_frame, Some(960));
        assert_eq!(plan.layout.total, 4);
        assert_eq!(plan.layout.start, 1);
        assert_eq!(plan.layout.count, 2);

        let plan = Plan::resolve(24_000, 4, &config).unwrap();
        assert_eq!(plan.start_frame, 240);
        assert_eq!(plan.end_frame, Some(480));

        // The channel selection is validated against the given channel count
        assert!(matches!(
            Plan::resolve(48_000, 2, &config),
            Err(ReadError::InvalidChannelRange {
                start: 1,
                count: 2,
                total: 2
            })
        ));

        let backwards = ReadConfig {
            start: Position::Frame(100),
            stop: Position::Frame(99),
            ..Default::default()
        };
        assert!(matches!(
            Plan::resolve(48_000, 1, &backwards),
            Err(ReadError::InvalidFrameRange {
                start: 100,
                end: 99
            })
        ));
    }

    /// A stop position must not bypass the resampling step.
    #[cfg(feature = "resample")]
    #[test]
    fn test_stop_with_resampling() {
        let sr_out: u32 = 24000;

        let audio = read::<f32>(
            "test_data/test_4ch.wav",
            ReadConfig {
                stop: Position::Frame(24000),
                sample_rate: Some(sr_out),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(audio.sample_rate, sr_out);
        assert_eq!(audio.num_channels, 4);
        // 24000 frames at 48 kHz are 12000 frames at 24 kHz
        assert_eq!(to_block(&audio).num_frames(), 12000);
    }

    /// The seek path is only taken for start offsets of more than one second.
    #[test]
    fn test_start_beyond_seek_threshold() {
        let path = crate::tmp_path("read-seek.wav");

        // Three seconds of a ramp, so that every frame is identifiable
        let num_frames = 48000 * 3;
        let mut samples = Vec::with_capacity(num_frames * 2);
        for frame in 0..num_frames {
            let value = frame as f32 / num_frames as f32;
            samples.push(value);
            samples.push(-value);
        }
        crate::writer::write(
            &path,
            &samples,
            2,
            48000,
            crate::writer::WriteConfig {
                sample_format: crate::writer::SampleFormat::Float32,
            },
        )
        .unwrap();

        for start in [48_001, 60_000, 100_000, 143_000] {
            let audio = read::<f32>(
                &path,
                ReadConfig {
                    start: Position::Frame(start),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(audio.num_channels, 2);
            assert_eq!(
                audio.samples_interleaved.len(),
                (num_frames - start) * 2,
                "wrong length for start frame {start}"
            );
            assert_eq!(
                audio.samples_interleaved[..2],
                samples[start * 2..start * 2 + 2],
                "wrong first frame for start frame {start}"
            );
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// What `read` documents about positions the file does not reach: a start
    /// beyond the end yields no samples, and a stop beyond the end clips to the
    /// frames that are there. Starts on both sides of the seek threshold are
    /// covered, because an unreachable start is what makes a seek fail.
    #[test]
    fn test_positions_beyond_the_end_of_the_file() {
        let path = crate::tmp_path("read-beyond-eof.wav");

        // Half a second, so that a start beyond the end can still be below the
        // one second seek threshold
        let num_frames = 24_000;
        let samples: Vec<f32> = (0..num_frames * 2).map(|i| i as f32 / 1e6).collect();
        crate::writer::write(
            &path,
            &samples,
            2,
            48000,
            crate::writer::WriteConfig {
                sample_format: crate::writer::SampleFormat::Float32,
            },
        )
        .unwrap();

        // One start per path to the same empty result: below the seek threshold
        // nothing is seeked, at 48_001 the seek aims one second earlier and lands
        // at the beginning of the file, and beyond the file the seek target
        // itself is out of range, so the read falls back to decoding.
        for start in [30_000, 48_001, 1_000_000] {
            let audio = read::<f32>(
                &path,
                ReadConfig {
                    start: Position::Frame(start),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(audio.num_channels, 2, "start frame {start}");
            assert_eq!(audio.sample_rate, 48000, "start frame {start}");
            assert!(
                audio.samples_interleaved.is_empty(),
                "start frame {start} returned {} samples",
                audio.samples_interleaved.len()
            );
        }

        // Nothing to resample, but the requested rate is still what the empty
        // audio is labelled with
        #[cfg(feature = "resample")]
        {
            let audio = read::<f32>(
                &path,
                ReadConfig {
                    start: Position::Frame(1_000_000),
                    sample_rate: Some(24_000),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(audio.sample_rate, 24_000);
            assert!(audio.samples_interleaved.is_empty());
        }

        let audio = read::<f32>(
            &path,
            ReadConfig {
                stop: Position::Frame(1_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(audio.samples_interleaved, samples);

        let audio = read::<f32>(
            &path,
            ReadConfig {
                stop: Position::Time(Duration::from_secs(60)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(audio.samples_interleaved, samples);

        std::fs::remove_file(&path).unwrap();
    }

    /// A time position that does not land on a frame boundary is rounded to the
    /// nearest frame instead of truncated, so that a position derived from a
    /// frame index does not move a frame earlier.
    #[test]
    fn test_sub_frame_time_positions_are_rounded() {
        // 1000.4 frames at 48 kHz
        let start = Duration::from_nanos(20_841_666);
        // 1200.5 frames, which truncation would place at 1200
        let stop = Duration::from_nanos(25_010_417);

        let audio = read::<f32>(
            "test_data/test_1ch.wav",
            ReadConfig {
                start: Position::Time(start),
                stop: Position::Time(stop),
                ..Default::default()
            },
        )
        .unwrap();

        let full = read::<f32>("test_data/test_1ch.wav", ReadConfig::default()).unwrap();
        assert_eq!(audio.samples_interleaved.len(), 201);
        assert_eq!(
            audio.samples_interleaved,
            full.samples_interleaved[1000..1201]
        );

        assert_eq!(position_to_frame(Position::Time(start), 48_000), Some(1000));
        assert_eq!(position_to_frame(Position::Time(stop), 48_000), Some(1201));
    }

    /// `read_block` is the same read, wrapped in an interleaved audio block.
    #[cfg(feature = "audio-blocks")]
    #[test]
    fn test_read_block_matches_read() {
        let config = || ReadConfig {
            start: Position::Frame(1_000),
            stop: Position::Frame(1_500),
            start_channel: Some(1),
            num_channels: Some(2),
            #[cfg(feature = "resample")]
            sample_rate: None,
        };

        let audio = read::<f32>("test_data/test_4ch.wav", config()).unwrap();
        let (block, sample_rate) = read_block::<f32>("test_data/test_4ch.wav", config()).unwrap();

        assert_eq!(sample_rate, audio.sample_rate);
        assert_eq!(block.num_channels(), audio.num_channels);
        assert_eq!(block.num_frames(), 500);
        assert_eq!(block.raw_data(), audio.samples_interleaved.as_slice());
    }

    /// A file without audio frames must report the declared channel layout and
    /// still validate the channel selection.
    #[test]
    fn test_read_file_without_frames() {
        let path = crate::tmp_path("read-empty.wav");
        crate::writer::write::<f32>(&path, &[], 2, 48000, crate::writer::WriteConfig::default())
            .unwrap();

        let audio = read::<f32>(&path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, 2);
        assert_eq!(audio.sample_rate, 48000);
        assert!(audio.samples_interleaved.is_empty());

        // Resampling nothing must not fail
        #[cfg(feature = "resample")]
        {
            let audio = read::<f32>(
                &path,
                ReadConfig {
                    sample_rate: Some(24000),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(audio.num_channels, 2);
            assert_eq!(audio.sample_rate, 24000);
            assert!(audio.samples_interleaved.is_empty());
        }

        // An invalid selection must be rejected even without any audio frames
        match read::<f32>(
            &path,
            ReadConfig {
                num_channels: Some(99),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidChannelRange { total: 2, .. }) => (),
            other => panic!("{other:?}"),
        }

        std::fs::remove_file(&path).unwrap();
    }
}
