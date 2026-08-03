use std::fs::File;
use std::path::Path;

use num::Float;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};
use thiserror::Error;

use crate::resample::{ResampleError, resample};

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
    #[error("could not read file")]
    Io(#[from] std::io::Error),

    #[error("could not decode audio")]
    Decode(#[from] symphonia::core::errors::Error),

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
    /// If specified the audio will be resampled to the given sample rate
    pub sample_rate: Option<u32>,
}

/// Upper bound for the pre-allocation derived from the container metadata, so
/// that a bogus frame count cannot request a huge allocation up front. The
/// buffer still grows beyond this if the file really is that long.
const MAX_PREALLOC_SAMPLES: usize = 16 * 1024 * 1024;

/// Read an audio file from disk.
///
/// Only the selected range is decoded and stored. `F` is the sample type of the
/// returned audio, either `f32` or `f64`, normalized to `[-1.0, 1.0]`.
///
/// The `stop` position of [`ReadConfig`] is exclusive, so reading from frame 100
/// to frame 200 yields 100 frames. A `start` position beyond the end of the file
/// yields no samples.
pub fn read<F: Float + rubato::Sample>(
    path: impl AsRef<Path>,
    config: ReadConfig,
) -> Result<Audio<F>, ReadError> {
    let decoded = decode::<F>(path.as_ref(), &config)?;

    let samples = match config.sample_rate {
        Some(sr_out) if sr_out != decoded.sample_rate => resample(
            &decoded.samples,
            decoded.num_channels,
            decoded.sample_rate,
            sr_out,
        )?,
        _ => decoded.samples,
    };

    Ok(Audio {
        samples_interleaved: samples,
        sample_rate: config.sample_rate.unwrap_or(decoded.sample_rate),
        num_channels: u16::try_from(decoded.num_channels)
            .map_err(|_| ReadError::TooManyChannels(decoded.num_channels))?,
    })
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
    /// Channels per frame in the file
    total: usize,
    /// First channel to extract
    start: usize,
    /// Number of channels to extract
    count: usize,
}

/// The audio track that is being read, with its parameters detached from the
/// format reader so that the reader can be borrowed mutably again.
struct TrackInfo {
    id: u32,
    params: AudioCodecParameters,
    sample_rate: u32,
    channels: Option<usize>,
    time_base: Option<TimeBase>,
    num_frames: Option<u64>,
}

/// Pick the audio track to read.
///
/// Prefers the track the container marks as default, and falls back to the first
/// audio track with a codec that can be decoded.
fn select_track(format: &dyn FormatReader) -> Result<TrackInfo, ReadError> {
    let track = format
        .default_track(TrackType::Audio)
        .filter(|track| track.codec_params.is_some())
        .or_else(|| format.first_track_known_codec(TrackType::Audio))
        .ok_or(ReadError::NoTrack)?;

    let params = track
        .codec_params
        .as_ref()
        .and_then(CodecParameters::audio)
        .ok_or(ReadError::NoTrack)?;

    Ok(TrackInfo {
        id: track.id,
        sample_rate: params.sample_rate.ok_or(ReadError::NoSampleRate)?,
        channels: params.channels.as_ref().map(Channels::count),
        params: params.clone(),
        time_base: track.time_base,
        num_frames: track.num_frames,
    })
}

fn decode<F: Float>(path: &Path, config: &ReadConfig) -> Result<Decoded<F>, ReadError> {
    let src = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }

    let meta_opts: MetadataOptions = Default::default();
    let fmt_opts: FormatOptions = Default::default();

    let mut format = symphonia::default::get_probe().probe(&hint, mss, fmt_opts, meta_opts)?;

    let mut track = select_track(&*format)?;
    let sample_rate = track.sample_rate;

    // Convert start/stop positions to frame numbers
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

    // Validate the channel selection up front when the container declares a
    // channel count. That way an invalid selection is rejected before anything
    // is decoded, and also for files that contain no audio packets at all.
    let declared = track
        .channels
        .map(|total| channel_range(config, total))
        .transpose()?;

    // Seek for large offsets to avoid decoding data that is thrown away again.
    // Below one second the seek overhead is not worth it, and decoding from the
    // beginning while discarding samples is simpler.
    if start_frame > sample_rate as usize
        && let Some(tb) = track.time_base
    {
        // An accurate seek always lands at or before the requested position, but
        // aim one second early anyway to give codecs with inter-frame
        // dependencies time to warm up. The frames in between are discarded
        // while decoding.
        let target = start_frame.saturating_sub(sample_rate as usize) as u64;
        let ts = i64::try_from(frames_to_ts(target, tb, sample_rate)).unwrap_or(i64::MAX);

        // Try to seek, but don't fail if seeking doesn't work. The stream
        // position is recovered from the packet timestamps either way.
        let _ = format.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(ts),
                track_id: track.id,
            },
        );
    }

    let dec_opts: AudioDecoderOptions = Default::default();
    let mut decoder =
        symphonia::default::get_codecs().make_audio_decoder(&track.params, &dec_opts)?;

    let mut samples: Vec<F> = Vec::new();
    let mut layout: Option<Layout> = None;

    // Sample rate reported by the first decoded packet. The decoded rate is
    // taken from the bitstream, so it may disagree with the container, but it
    // must stay the same for every packet that follows.
    let mut decoded_rate: Option<u32> = None;

    // Reused per packet to hold the selected frames of the decoded audio.
    // `f64` is used because it can hold every sample format symphonia decodes
    // to without losing precision.
    let mut scratch: Vec<f64> = Vec::new();

    // Reserve up front when the container reports a frame count, so that long
    // reads don't repeatedly reallocate a growing buffer.
    if let Some((_, ch_count)) = declared
        && let Some(frames) = expected_frames(start_frame, end_frame, track.num_frames)
    {
        samples.reserve(frames.saturating_mul(ch_count).min(MAX_PREALLOC_SAMPLES));
    }

    // Absolute frame index of the next frame to be decoded. Streams with a time
    // base take their position from the packet timestamps instead, and only use
    // this to continue the timeline across a chained stream.
    let mut position: Option<u64> = None;

    // Offset applied to packet timestamps. Stays zero unless a chained stream
    // restarts its timestamps, in which case it continues where the previous
    // stream ended.
    let mut stream_base = 0u64;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            // The end of the media was reached.
            Ok(None) => break,
            // The track list changed, which happens for chained streams such as
            // concatenated OGG files. The decoder has to be rebuilt from the new
            // track list, and reading only continues if the new track is
            // compatible with what has been decoded so far.
            Err(Error::ResetRequired) => {
                let next = select_track(&*format)?;
                if next.sample_rate != sample_rate {
                    return Err(ReadError::SampleRateChanged {
                        expected: sample_rate,
                        found: next.sample_rate,
                    });
                }
                decoder =
                    symphonia::default::get_codecs().make_audio_decoder(&next.params, &dec_opts)?;
                track = next;
                // The new stream restarts its timestamps at zero, so continue
                // the timeline where the previous stream ended.
                stream_base = position.unwrap_or(0);
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        if packet.track_id != track.id {
            continue;
        }

        // Frames before the presentation timestamp are encoder delay that the
        // decoder discards, so the trimmed buffer starts at `pts + trim_start`.
        let packet_ts = packet
            .pts
            .get()
            .saturating_add_unsigned(packet.trim_start.get())
            .max(0) as u64;

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A malformed packet is discardable and decoding may continue with
            // the next one, but only if the position can be recovered from its
            // timestamp. Without a time base the discarded frames would shift
            // everything that follows, so the error is propagated instead.
            Err(Error::DecodeError(_) | Error::IoError(_)) if track.time_base.is_some() => {
                continue;
            }
            // The audio specification of the decoded audio may change after a
            // reset, which is picked up from the next packet.
            Err(Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        // The decoded specification is authoritative for the channel count and
        // the sample rate, the container hint may disagree with them. Both have
        // to stay stable, otherwise the frames of this packet do not belong to
        // the same stream as everything that was decoded before.
        let packet_rate = decoded.spec().rate();
        match decoded_rate {
            Some(known) if known != packet_rate => {
                return Err(ReadError::SampleRateChanged {
                    expected: known,
                    found: packet_rate,
                });
            }
            Some(_) => (),
            None => decoded_rate = Some(packet_rate),
        }

        let total_channels = decoded.spec().channels().count();
        let layout = match layout {
            Some(known) if known.total != total_channels => {
                return Err(ReadError::ChannelCountChanged {
                    expected: known.total,
                    found: total_channels,
                });
            }
            Some(known) => known,
            None => {
                let (start, count) = channel_range(config, total_channels)?;
                *layout.insert(Layout {
                    total: total_channels,
                    start,
                    count,
                })
            }
        };

        let packet_frames = decoded.frames();

        // The timestamp states where these frames belong, which is more robust
        // than counting decoded frames: a decoder may return fewer frames than
        // the packet covers, for example while warming up after a seek.
        let packet_start = match track.time_base {
            Some(tb) => stream_base + ts_to_frames(packet_ts, tb, sample_rate),
            None => position.unwrap_or(0),
        };
        let packet_end = packet_start + packet_frames as u64;

        // Intersect the packet with the requested frame range
        let copy_start = packet_start.max(start_frame as u64);
        let copy_end = match end_frame {
            Some(end) => packet_end.min(end as u64),
            None => packet_end,
        };

        if copy_start < copy_end {
            let first = (copy_start - packet_start) as usize;
            let last = (copy_end - packet_start) as usize;

            // Convert whatever sample format the codec produced into the
            // scratch buffer, then take the selected frames out of it.
            scratch.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved::<f64, _>(scratch.as_mut_slice());
            let frames = &scratch[first * layout.total..last * layout.total];

            if layout.start == 0 && layout.count == layout.total {
                // All channels are selected, so nothing has to be dropped
                extend_samples(&mut samples, frames);
            } else {
                for frame in frames.chunks_exact(layout.total) {
                    let selected = &frame[layout.start..layout.start + layout.count];
                    extend_samples(&mut samples, selected);
                }
            }
        }

        position = Some(packet_end);

        if let Some(end) = end_frame
            && packet_end >= end as u64
        {
            break;
        }
    }

    // Fall back to the declared channel count for files without audio packets,
    // so that an empty selection still reports a sane layout.
    let num_channels = match (layout, declared) {
        (Some(layout), _) => layout.count,
        (None, Some((_, count))) => count,
        (None, None) => return Err(ReadError::NoChannels),
    };

    Ok(Decoded {
        samples,
        num_channels,
        sample_rate,
    })
}

fn extend_samples<F: Float>(samples: &mut Vec<F>, src: &[f64]) {
    // `F` is `f32` or `f64` here, so the conversion cannot fail
    samples.extend(src.iter().map(|&s| F::from(s).unwrap_or_else(F::zero)));
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

/// Number of frames the read is expected to yield, if the file length is known.
fn expected_frames(start: usize, end: Option<usize>, n_frames: Option<u64>) -> Option<usize> {
    let total = n_frames.map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let end = match (end, total) {
        (Some(end), Some(total)) => end.min(total),
        (Some(end), None) => end,
        (None, Some(total)) => total,
        (None, None) => return None,
    };
    Some(end.saturating_sub(start))
}

/// Frame index that the timestamp `ts` refers to.
fn ts_to_frames(ts: u64, tb: TimeBase, sample_rate: u32) -> u64 {
    let dividend = ts as u128 * u128::from(tb.numer.get()) * u128::from(sample_rate);
    (dividend / u128::from(tb.denom.get())) as u64
}

/// Timestamp that refers to the frame at index `frame`.
fn frames_to_ts(frame: u64, tb: TimeBase, sample_rate: u32) -> u64 {
    let dividend = frame as u128 * u128::from(tb.denom.get());
    (dividend / (u128::from(tb.numer.get()) * u128::from(sample_rate))) as u64
}

#[cfg(feature = "audio-blocks")]
pub fn read_block<F: num::Float + 'static + rubato::Sample>(
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
    use std::time::Duration;

    use audio_blocks::{AudioBlock, InterleavedView};

    use super::*;

    fn to_block<F: num::Float + 'static>(audio: &Audio<F>) -> InterleavedView<'_, F> {
        InterleavedView::from_slice(&audio.samples_interleaved, audio.num_channels)
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

    /// A stop position must not bypass the resampling step.
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
        let path = "tmp_read_seek.wav";

        // Three seconds of a ramp, so that every frame is identifiable
        let num_frames = 48000 * 3;
        let mut samples = Vec::with_capacity(num_frames * 2);
        for frame in 0..num_frames {
            let value = frame as f32 / num_frames as f32;
            samples.push(value);
            samples.push(-value);
        }
        crate::writer::write(
            path,
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
                path,
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

        std::fs::remove_file(path).unwrap();
    }

    /// A file without audio frames must report the declared channel layout and
    /// still validate the channel selection.
    #[test]
    fn test_read_file_without_frames() {
        let path = "tmp_read_empty.wav";
        crate::writer::write::<f32>(path, &[], 2, 48000, crate::writer::WriteConfig::default())
            .unwrap();

        let audio = read::<f32>(path, ReadConfig::default()).unwrap();
        assert_eq!(audio.num_channels, 2);
        assert_eq!(audio.sample_rate, 48000);
        assert!(audio.samples_interleaved.is_empty());

        // Resampling nothing must not fail
        let audio = read::<f32>(
            path,
            ReadConfig {
                sample_rate: Some(24000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(audio.num_channels, 2);
        assert_eq!(audio.sample_rate, 24000);
        assert!(audio.samples_interleaved.is_empty());

        // An invalid selection must be rejected even without any audio frames
        match read::<f32>(
            path,
            ReadConfig {
                num_channels: Some(99),
                ..Default::default()
            },
        ) {
            Err(ReadError::InvalidChannelRange { total: 2, .. }) => (),
            other => panic!("{other:?}"),
        }

        std::fs::remove_file(path).unwrap();
    }
}
