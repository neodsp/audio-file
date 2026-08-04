use std::fs::File;
use std::path::Path;

use num::Float;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
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
    sample_rate: u32,
    channels: Option<usize>,
    time_base: Option<TimeBase>,
    num_frames: Option<u64>,
}

type SelectedTrack = (TrackInfo, Box<dyn AudioDecoder>);

/// Pick the audio track to read and construct its decoder.
///
/// Prefers the track the container marks as default, and falls back through the
/// remaining audio tracks until a decoder can be constructed.
fn select_track(
    format: &dyn FormatReader,
    dec_opts: &AudioDecoderOptions,
) -> Result<SelectedTrack, ReadError> {
    let default = format.default_track(TrackType::Audio);
    let default_id = default.map(|track| track.id);
    let candidates = default.into_iter().chain(
        format
            .tracks()
            .iter()
            .filter(|track| Some(track.id) != default_id),
    );
    let mut first_error = None;

    for track in candidates {
        match prepare_track(track, dec_opts) {
            Ok(Some(selected)) => return Ok(selected),
            Ok(None) => (),
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }

    Err(first_error.unwrap_or(ReadError::NoTrack))
}

/// Extract the metadata needed by the reader and verify that the track really
/// has a registered decoder. `None` means this is not a usable audio track.
fn prepare_track(
    track: &Track,
    dec_opts: &AudioDecoderOptions,
) -> Result<Option<SelectedTrack>, ReadError> {
    let Some(params) = track.codec_params.as_ref().and_then(CodecParameters::audio) else {
        return Ok(None);
    };

    if params.codec == CODEC_ID_NULL_AUDIO {
        return Ok(None);
    }

    let decoder = symphonia::default::get_codecs().make_audio_decoder(params, dec_opts)?;
    let info = TrackInfo {
        id: track.id,
        sample_rate: params.sample_rate.ok_or(ReadError::NoSampleRate)?,
        channels: params.channels.as_ref().map(Channels::count),
        time_base: track.time_base,
        num_frames: track.num_frames,
    };

    Ok(Some((info, decoder)))
}

fn open_format(path: &Path) -> Result<Box<dyn FormatReader>, ReadError> {
    let src = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }

    Ok(symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?)
}

fn decode<F: Float>(path: &Path, config: &ReadConfig) -> Result<Decoded<F>, ReadError> {
    let mut format = open_format(path)?;
    let dec_opts: AudioDecoderOptions = Default::default();
    let (mut track, mut decoder) = select_track(&*format, &dec_opts)?;
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

    // The decoded specification is authoritative when packets are available.
    // Retain the container declaration only as a fallback for files without any
    // decoded audio, rather than rejecting a selection against a possibly stale
    // metadata value before decoding starts.
    let declared_channels = track.channels;

    // Seek for large offsets to avoid decoding data that is thrown away again.
    // Below one second the seek overhead is not worth it, and decoding from the
    // beginning while discarding samples is simpler.
    let mut seeked = false;
    if start_frame > sample_rate as usize
        && let Some(tb) = track.time_base
        && time_base_has_exact_frames(tb, sample_rate)
        // Symphonia's Matroska accurate seek may land several seconds after its
        // target. Disable it explicitly until the demuxer can guarantee a safe
        // landing; decoding from the beginning is slower but frame-correct.
        && format.format_info().short_name != "matroska"
    {
        // Aim one second early to give codecs with inter-frame dependencies time
        // to warm up. Some format readers may still land after the requested
        // start despite `SeekMode::Accurate`; such a seek cannot produce the full
        // requested range and is discarded below.
        let target = start_frame.saturating_sub(sample_rate as usize) as u64;
        let ts = i64::try_from(frames_to_ts(target, tb, sample_rate)).unwrap_or(i64::MAX);

        let seek_result = format.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(ts),
                track_id: track.id,
            },
        );

        if let Ok(result) = seek_result
            && seek_landing_is_safe(result.actual_ts.get(), start_frame as u64, tb, sample_rate)
        {
            seeked = true;
        } else {
            // A failed seek is not guaranteed to leave every format reader at its
            // original position. Reopening is also the only reliable way to undo
            // a successful seek that overshot the requested start.
            format = open_format(path)?;
            (track, decoder) = select_track(&*format, &dec_opts)?;
        }
    }

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
    if let Some(ch_count) = declared_channels
        .and_then(|total| channel_range(config, total).ok().map(|(_, count)| count))
        && let Some(frames) = expected_frames(start_frame, end_frame, track.num_frames)
    {
        samples.reserve(frames.saturating_mul(ch_count).min(MAX_PREALLOC_SAMPLES));
    }

    // Absolute frame index of the next decoded frame. Decoding from the beginning
    // has an exact zero anchor. After a successful seek (or a discontinuity), one
    // packet timestamp establishes a new anchor; decoded frame counts advance it
    // from then on. Re-anchoring every packet would turn timestamp quantization
    // into overlaps or gaps.
    let mut position = if seeked { None } else { Some(0) };
    // Offset for re-anchoring within a chained stream, whose packet timestamps
    // restart at zero even though its decoded frames continue the output.
    let mut stream_base = 0u64;
    // Never copy the same absolute frame twice if timestamp-based recovery after
    // a decode error lands before data that was already returned.
    let mut copied_until = start_frame as u64;

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
                let (next, next_decoder) = select_track(&*format, &dec_opts)?;
                if next.sample_rate != sample_rate {
                    return Err(ReadError::SampleRateChanged {
                        expected: sample_rate,
                        found: next.sample_rate,
                    });
                }
                decoder = next_decoder;
                track = next;
                // Keep `position` for normal decoding, and retain the boundary as
                // the base if a later error requires re-anchoring from this new
                // stream's zero-based timestamps.
                stream_base = position.unwrap_or(stream_base);
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        if packet.track_id != track.id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A malformed packet is discardable and decoding may continue with
            // the next one, but only if the position can be recovered from its
            // timestamp. Without a time base the discarded frames would shift
            // everything that follows, so the error is propagated instead.
            Err(Error::DecodeError(_) | Error::IoError(_)) if track.time_base.is_some() => {
                // The number of frames lost with this packet is unknown. Let the
                // next non-empty packet establish a new position instead of
                // shifting all later decoded frames by the missing amount.
                position = None;
                continue;
            }
            // The audio specification of the decoded audio may change after a
            // reset, which is picked up from the next packet.
            Err(Error::ResetRequired) => {
                decoder.reset();
                position = None;
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

        // Decoder output is continuous after it has been anchored. At a
        // discontinuity, empty warm-up buffers cannot identify the position of
        // later output, so wait for the first packet that actually emits frames.
        let packet_start = match position {
            Some(position) => position,
            None if packet_frames == 0 => continue,
            None => match track.time_base {
                Some(tb) => stream_base.saturating_add(timestamp_to_frame(
                    packet.pts.get(),
                    packet.trim_start.get(),
                    tb,
                    sample_rate,
                )),
                None => stream_base,
            },
        };
        let packet_end = packet_start.saturating_add(packet_frames as u64);

        // Intersect the packet with the requested frame range
        let copy_start = packet_start.max(start_frame as u64).max(copied_until);
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
            copied_until = copy_end;
        }

        position = Some(packet_end);

        if let Some(end) = end_frame
            && packet_end >= end as u64
        {
            break;
        }
    }

    // Fall back to and validate against the declared channel count only if no
    // decoded specification was available (for example, an empty WAV file).
    let num_channels = match layout {
        Some(layout) => layout.count,
        None => declared_channels
            .map(|total| channel_range(config, total))
            .transpose()?
            .map(|(_, count)| count)
            .ok_or(ReadError::NoChannels)?,
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

/// Frame index that a signed timestamp and subsequent frame trim refer to.
fn timestamp_to_frame(ts: i64, trim_start: u64, tb: TimeBase, sample_rate: u32) -> u64 {
    let frames = i128::from(ts) * i128::from(tb.numer.get()) * i128::from(sample_rate)
        / i128::from(tb.denom.get())
        + i128::from(trim_start);
    u64::try_from(frames).unwrap_or(if frames < 0 { 0 } else { u64::MAX })
}

/// Whether each timestamp tick maps to an exact audio-frame boundary.
fn time_base_has_exact_frames(tb: TimeBase, sample_rate: u32) -> bool {
    let frames_per_tick_numer = u128::from(tb.numer.get()) * u128::from(sample_rate);
    frames_per_tick_numer.is_multiple_of(u128::from(tb.denom.get()))
}

/// Whether a seek landed early enough to decode the requested start frame.
fn seek_landing_is_safe(actual_ts: i64, start_frame: u64, tb: TimeBase, sample_rate: u32) -> bool {
    timestamp_to_frame(actual_ts, 0, tb, sample_rate) <= start_frame
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

    #[test]
    fn test_exact_frame_time_bases() {
        assert!(time_base_has_exact_frames(
            TimeBase::try_new(1, 1_000).unwrap(),
            48_000
        ));
        assert!(time_base_has_exact_frames(
            TimeBase::try_new(1, 48_000).unwrap(),
            48_000
        ));
        assert!(!time_base_has_exact_frames(
            TimeBase::try_new(1, 1_000).unwrap(),
            44_100
        ));
        assert!(!time_base_has_exact_frames(
            TimeBase::try_new(1, 96_000).unwrap(),
            48_000
        ));
    }

    #[test]
    fn test_seek_landing_must_not_overshoot_start() {
        let tb = TimeBase::try_new(1, 1_000).unwrap();
        assert!(seek_landing_is_safe(1_250, 60_000, tb, 48_000));
        assert!(!seek_landing_is_safe(1_251, 60_000, tb, 48_000));
    }

    #[cfg(any(
        feature = "all-codecs",
        all(feature = "mkv", feature = "flac"),
        all(feature = "vorbis", any(feature = "mkv", feature = "ogg"))
    ))]
    fn assert_ranges_match_full_decode(path: &str, ranges: &[std::ops::Range<usize>]) {
        let full = read::<f32>(path, ReadConfig::default()).unwrap();

        for range in ranges {
            let selected = read::<f32>(
                path,
                ReadConfig {
                    start: Position::Frame(range.start),
                    stop: Position::Frame(range.end),
                    ..Default::default()
                },
            )
            .unwrap();

            let expected = &full.samples_interleaved[range.clone()];
            assert!(
                selected.samples_interleaved.len() <= range.len(),
                "{path}: range {range:?} returned too many frames"
            );
            assert_eq!(
                selected.samples_interleaved, expected,
                "{path}: range {range:?} did not match the full decode"
            );
        }
    }

    /// The container marks an AC-3 track as default and symphonia has no AC-3
    /// decoder, so the track exposes complete audio parameters yet cannot be
    /// decoded. Selecting it on its parameters alone made the whole file
    /// unreadable; the decodable PCM track must be used instead.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_unusable_default_track_falls_back_to_decodable_track() {
        const PATH: &str = "test_data/test_unusable_default.mka";

        // Assert the fixture still poses the problem. Without this the test
        // would keep passing if the default track ever stopped being reported
        // as an audio track with an undecodable codec.
        let format = open_format(Path::new(PATH)).unwrap();
        let params = format
            .default_track(TrackType::Audio)
            .and_then(|track| track.codec_params.as_ref())
            .and_then(CodecParameters::audio)
            .expect("fixture: default audio track must expose audio parameters");
        assert!(
            symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .is_err(),
            "fixture: default track is decodable, so it no longer exercises the fallback"
        );

        let audio = read::<f32>(PATH, ReadConfig::default()).unwrap();

        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.num_channels, 1);
        assert_eq!(audio.samples_interleaved.len(), 960);
    }

    /// The Matroska track metadata declares mono while the FLAC stream info
    /// declares stereo. Channel selection must follow the decoded FLAC layout.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_channel_selection_uses_decoded_layout() {
        const PATH: &str = "test_data/test_declared_mono_decoded_stereo.mka";

        // Assert the fixture still contains the intended metadata disagreement.
        let format = open_format(Path::new(PATH)).unwrap();
        let declared_channels = format
            .default_track(TrackType::Audio)
            .and_then(|track| track.codec_params.as_ref())
            .and_then(CodecParameters::audio)
            .and_then(|params| params.channels.as_ref())
            .map(Channels::count);
        assert_eq!(declared_channels, Some(1));

        let full = read::<f32>(PATH, ReadConfig::default()).unwrap();
        assert_eq!(full.num_channels, 2);
        assert!(!full.samples_interleaved.is_empty());

        // This selection is invalid against the declared mono layout but valid
        // against the authoritative decoded stereo layout.
        let selected = read::<f32>(
            PATH,
            ReadConfig {
                start_channel: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(selected.num_channels, 1);
        assert_eq!(
            selected.samples_interleaved,
            full.samples_interleaved
                .chunks_exact(2)
                .map(|frame| frame[1])
                .collect::<Vec<_>>()
        );
    }

    /// Chained Ogg streams may replace the decoder and decoded layout. A real
    /// channel-count change cannot be represented in one interleaved output.
    #[cfg(all(
        any(feature = "all-codecs", feature = "ogg"),
        any(feature = "all-codecs", feature = "vorbis")
    ))]
    #[test]
    fn test_mid_stream_channel_count_change_is_rejected() {
        let error = read::<f32>(
            "test_data/test_channel_count_change.ogg",
            ReadConfig::default(),
        )
        .expect_err("the chained stream changes from mono to stereo");

        assert!(
            matches!(
                error,
                ReadError::ChannelCountChanged {
                    expected: 1,
                    found: 2
                }
            ),
            "{error:?}"
        );
    }

    /// Matroska timestamps are millisecond-based and therefore cannot position
    /// individual 44.1 kHz decoded buffers without rounding errors.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_flac_matroska_frame_ranges() {
        assert_ranges_match_full_decode(
            "test_data/test_flac.mka",
            &[0..100, 4521..4621, 60_000..60_100],
        );
    }

    /// Vorbis emits empty warm-up buffers before its first playable frames.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "vorbis")
    ))]
    #[test]
    fn test_vorbis_matroska_frame_ranges() {
        assert_ranges_match_full_decode(
            "test_data/test_vorbis.mka",
            &[0..100, 4521..4621, 60_000..60_100],
        );
    }

    /// A seeked Vorbis decoder emits an empty warm-up buffer whose packet does
    /// not own the first frames emitted by the following packet.
    #[cfg(all(
        any(feature = "all-codecs", feature = "ogg"),
        any(feature = "all-codecs", feature = "vorbis")
    ))]
    #[test]
    fn test_vorbis_ogg_seeked_frame_ranges() {
        assert_ranges_match_full_decode(
            "test_data/test_vorbis.ogg",
            &[60_000..60_100, 100_000..100_100, 140_000..140_100],
        );
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
