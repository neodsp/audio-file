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
        num_channels: checked_num_channels(decoded.num_channels)?,
    })
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

    /// Number of frames the read is expected to yield, if the file length is
    /// known.
    fn expected_frames(&self, num_frames: Option<u64>) -> Option<usize> {
        let total = num_frames.map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        let end = match (self.end_frame, total) {
            (Some(end), Some(total)) => end.min(total),
            (Some(end), None) => end,
            (None, Some(total)) => total,
            (None, None) => return None,
        };
        Some(end.saturating_sub(self.start_frame))
    }
}

/// Plan for a file without a single decodable packet, for example an empty WAV
/// file. The container declaration is all there is, and the config is still
/// validated against it so that an invalid selection is rejected.
fn declared_plan(track: &TrackInfo, config: &ReadConfig) -> Result<Plan, ReadError> {
    let channels = track.channels.ok_or(ReadError::NoChannels)?;
    Plan::resolve(track.sample_rate, channels, config)
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

    let mut samples: Vec<F> = Vec::new();

    // Reused per packet to hold the selected frames of the decoded audio.
    // `f64` is used because it can hold every sample format symphonia decodes
    // to without losing precision.
    let mut scratch: Vec<f64> = Vec::new();

    // Resolved from the first packet that decodes, see `Plan`.
    let mut plan: Option<Plan> = None;
    let mut seeked = false;

    // Absolute frame index of the next decoded frame. Decoding from the beginning
    // has an exact zero anchor. After a successful seek (or a discontinuity), one
    // packet timestamp establishes a new anchor; decoded frame counts advance it
    // from then on. Re-anchoring every packet would turn timestamp quantization
    // into overlaps or gaps.
    let mut position = Some(0);
    // Offset for re-anchoring within a chained stream, whose packet timestamps
    // restart at zero even though its decoded frames continue the output.
    let mut stream_base = 0u64;
    // Set while a seek landing still has to be checked against the requested
    // start, which can only be done once a packet after it has been positioned.
    let mut unverified_landing = false;
    // Last position that was known exactly before a discarded packet left a hole,
    // and the origin of the grid the next position is snapped to. Only a discarded
    // packet keeps the timeline around the hole intact; a seek or a decoder reset
    // is a real discontinuity, where no grid carries over.
    let mut grid_anchor: Option<u64> = None;
    // Never copy the same absolute frame twice if timestamp-based recovery after
    // a decode error lands before data that was already returned. Set to the
    // start frame as soon as the plan is resolved.
    let mut copied_until = 0u64;

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
                if let Some(plan) = plan
                    && next.sample_rate != plan.sample_rate
                {
                    return Err(ReadError::SampleRateChanged {
                        expected: plan.sample_rate,
                        found: next.sample_rate,
                    });
                }
                decoder = next_decoder;
                track = next;
                // Keep `position` for normal decoding, and retain the boundary as
                // the base if a later error requires re-anchoring from this new
                // stream's zero-based timestamps.
                stream_base = position.unwrap_or(stream_base);
                grid_anchor = None;
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
                // shifting all later decoded frames by the missing amount. Keep
                // the position before the hole as the grid origin, including
                // across a run of several discarded packets.
                grid_anchor = position.or(grid_anchor);
                position = None;
                continue;
            }
            // The audio specification of the decoded audio may change after a
            // reset, which is picked up from the next packet.
            Err(Error::ResetRequired) => {
                decoder.reset();
                position = None;
                // The reset is a discontinuity, not a hole in an otherwise intact
                // timeline.
                grid_anchor = None;
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let packet_rate = decoded.spec().rate();
        let packet_channels = decoded.spec().channels().count();

        let plan = match plan {
            // The decoded specification has to stay stable, otherwise the frames
            // of this packet do not belong to the same stream as everything that
            // was decoded before. A rate that differs also invalidates every
            // frame position that was resolved from it.
            Some(known) => {
                if packet_rate != known.sample_rate {
                    return Err(ReadError::SampleRateChanged {
                        expected: known.sample_rate,
                        found: packet_rate,
                    });
                }
                if packet_channels != known.layout.total {
                    return Err(ReadError::ChannelCountChanged {
                        expected: known.layout.total,
                        found: packet_channels,
                    });
                }
                known
            }
            // The first decoded packet is what the whole read is resolved
            // against. A warm-up packet without any frames already carries the
            // specification the decoder was configured with, so it resolves the
            // plan just as well as a packet with audio in it.
            None => {
                let resolved = Plan::resolve(packet_rate, packet_channels, config)?;
                copied_until = resolved.start_frame as u64;

                // Reserve up front when the container reports a frame count, so
                // that long reads don't repeatedly reallocate a growing buffer.
                if let Some(frames) = resolved.expected_frames(track.num_frames) {
                    samples.reserve(
                        frames
                            .saturating_mul(resolved.layout.count)
                            .min(MAX_PREALLOC_SAMPLES),
                    );
                }

                // Seeking can only be decided here, because the frame positions
                // it needs are resolved against the sample rate of this packet.
                // The frames of this packet are given up, which loses nothing:
                // decoding resumes at or before the requested start, so anything
                // this packet carried inside the requested range is decoded
                // again. It is usually the first packet of the file and lies
                // entirely before the seek target, but a file whose early
                // packets all fail to decode resolves the plan later than that.
                if let Some(tb) = track.time_base
                    && should_seek(
                        resolved.start_frame,
                        resolved.sample_rate,
                        tb,
                        format.format_info().short_name,
                    )
                {
                    if seek_before(
                        &mut *format,
                        track.id,
                        resolved.start_frame,
                        resolved.sample_rate,
                        tb,
                    ) {
                        // The decoder keeps state from before the landing, and
                        // the frames after it can no longer be counted from the
                        // beginning of the file.
                        decoder.reset();
                        position = None;
                        seeked = true;
                        unverified_landing = true;
                        // The landing is a discontinuity, so a hole from before it
                        // cannot put the frames after it on a grid.
                        grid_anchor = None;
                    } else {
                        // A failed seek is not guaranteed to leave every format
                        // reader at its original position. Reopening is also the
                        // only reliable way to undo a successful seek that
                        // overshot the requested start, and it decodes this
                        // packet again from a known position.
                        format = open_format(path)?;
                        (track, decoder) = select_track(&*format, &dec_opts)?;
                        position = Some(0);
                        // Everything observed during the abandoned attempt refers
                        // to positions this read no longer passes through.
                        grid_anchor = None;
                    }
                    plan = Some(resolved);
                    continue;
                }

                *plan.insert(resolved)
            }
        };

        let layout = plan.layout;
        let packet_frames = decoded.frames();

        // Decoder output is continuous after it has been anchored. At a
        // discontinuity, empty warm-up buffers cannot identify the position of
        // later output, so wait for the first packet that actually emits frames.
        let packet_start = match position {
            Some(position) => position,
            None if packet_frames == 0 => continue,
            None => match track.time_base {
                Some(tb) => {
                    let estimate = stream_base.saturating_add(timestamp_to_frame(
                        packet.pts.get(),
                        packet.trim_start.get(),
                        tb,
                        plan.sample_rate,
                    ));
                    // A timestamp coarser than one frame locates this packet only
                    // to within one tick. If a discarded packet is all that stands
                    // between it and a position that was known exactly, and the
                    // codec spaces its packets evenly, the exact position can be
                    // recovered from that grid instead of from the timestamp.
                    match grid_anchor {
                        Some(anchor) => snap_to_grid(
                            estimate,
                            anchor,
                            packet_frames as u64,
                            frames_per_tick(tb, plan.sample_rate),
                        ),
                        None => estimate,
                    }
                }
                None => stream_base,
            },
        };

        // A format reader can report a landing at or before the requested start and
        // still deliver its first packet after it. The frames in between were never
        // lost, so they are not filled with silence, and the read would quietly
        // begin late and come up short. Decoding from the beginning always reaches
        // the requested start, which is what not seeking would have cost anyway.
        if std::mem::take(&mut unverified_landing) && packet_start > plan.start_frame as u64 {
            format = open_format(path)?;
            (track, decoder) = select_track(&*format, &dec_opts)?;
            position = Some(0);
            seeked = false;
            grid_anchor = None;
            continue;
        }

        let packet_end = packet_start.saturating_add(packet_frames as u64);

        // Intersect the packet with the requested frame range
        let copy_start = packet_start.max(plan.start_frame as u64).max(copied_until);
        let copy_end = match plan.end_frame {
            Some(end) => packet_end.min(end as u64),
            None => packet_end,
        };

        if copy_start < copy_end {
            // Frames that were lost with a discarded packet leave a hole. Filling
            // it with silence keeps every later frame at its own position in the
            // output, instead of shifting the whole remainder of the read earlier
            // by the number of missing frames.
            //
            // A hole before the first copied frame is only filled when the read
            // was not seeked. After a seek the first packet may simply start
            // later than requested, and its frames were never lost, so silence
            // would be presented as audio that the file does have.
            if copy_start > copied_until && (!seeked || copied_until > plan.start_frame as u64) {
                let missing = fill_frames(
                    copy_start - copied_until,
                    samples.len() / layout.count,
                    layout.count,
                    plan.expected_frames(track.num_frames),
                );
                let len = samples
                    .len()
                    .saturating_add(missing.saturating_mul(layout.count));
                samples.resize(len, F::zero());
            }

            let first = (copy_start - packet_start) as usize;
            let last = (copy_end - packet_start) as usize;

            // Convert whatever sample format the codec produced into the
            // scratch buffer, then take the selected frames out of it.
            scratch.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved::<f64, _>(scratch.as_mut_slice());
            append_selected(&mut samples, &scratch, layout, first..last);

            copied_until = copy_end;
        }

        position = Some(packet_end);

        if let Some(end) = plan.end_frame
            && packet_end >= end as u64
        {
            break;
        }
    }

    let plan = match plan {
        Some(plan) => plan,
        None => declared_plan(&track, config)?,
    };

    Ok(Decoded {
        samples,
        num_channels: plan.layout.count,
        sample_rate: plan.sample_rate,
    })
}

/// Append the selected channels of the frames `frames` of an interleaved buffer
/// to the output.
fn append_selected<F: Float>(
    samples: &mut Vec<F>,
    interleaved: &[f64],
    layout: Layout,
    frames: std::ops::Range<usize>,
) {
    let selected = &interleaved[frames.start * layout.total..frames.end * layout.total];

    if layout.start == 0 && layout.count == layout.total {
        // All channels are selected, so nothing has to be dropped
        extend_samples(samples, selected);
    } else {
        for frame in selected.chunks_exact(layout.total) {
            extend_samples(samples, &frame[layout.start..layout.start + layout.count]);
        }
    }
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

/// Number of silent frames to insert for a hole of `missing` frames, after
/// `written` frames have been produced.
///
/// A corrupt timestamp could ask for an enormous hole, so the fill is capped by
/// the frames the read can produce at all, and by the same budget as the
/// preallocation when neither the stop position nor the file length bounds it. A
/// capped fill leaves the output short, which a truncated file does as well.
fn fill_frames(missing: u64, written: usize, channels: usize, max_frames: Option<usize>) -> usize {
    let missing = usize::try_from(missing).unwrap_or(usize::MAX);
    let limit = match max_frames {
        Some(max) => max.saturating_sub(written),
        None => MAX_PREALLOC_SAMPLES / channels.max(1),
    };
    missing.min(limit)
}

/// Frame index that a signed timestamp and subsequent frame trim refer to.
fn timestamp_to_frame(ts: i64, trim_start: u64, tb: TimeBase, sample_rate: u32) -> u64 {
    let frames = i128::from(ts) * i128::from(tb.numer.get()) * i128::from(sample_rate)
        / i128::from(tb.denom.get())
        + i128::from(trim_start);
    u64::try_from(frames).unwrap_or(if frames < 0 { 0 } else { u64::MAX })
}

/// Largest number of frames one timestamp tick can span, which bounds how far a
/// timestamp can be from the frame it refers to.
fn frames_per_tick(tb: TimeBase, sample_rate: u32) -> u64 {
    let numer = u128::from(tb.numer.get()) * u128::from(sample_rate);
    u64::try_from(numer.div_ceil(u128::from(tb.denom.get()))).unwrap_or(u64::MAX)
}

/// Round a position estimate onto the packet grid that starts at `anchor`.
///
/// A timestamp coarser than one frame, such as the milliseconds of a Matroska file
/// at 44.1 kHz, locates a packet only to within one tick. A codec with a constant
/// blocksize spaces its packets `block` frames apart, so the true position is a
/// whole number of blocks after a position that is already known exactly, and the
/// estimate can be rounded back onto it.
///
/// `block` is the frame count of the packet being positioned, which is only the
/// real spacing if the codec has a constant blocksize and this is not its last,
/// short packet. `tolerance` is what makes that guess safe: an estimate further
/// than one timestamp tick from the nearest grid point is returned unchanged,
/// because timestamp quantization cannot account for the difference. A spacing that
/// is not the real one is therefore either rejected, or off by less than a tick,
/// which is what the estimate already was.
fn snap_to_grid(estimate: u64, anchor: u64, block: u64, tolerance: u64) -> u64 {
    // Only a position ahead of the anchor can sit on the grid ahead of it.
    let Some(ahead) = estimate.checked_sub(anchor) else {
        return estimate;
    };
    // At least one packet was lost to get here, so the nearest grid point can
    // never be the anchor itself.
    let blocks = ahead
        .saturating_add(block / 2)
        .checked_div(block)
        .unwrap_or(1)
        .max(1);
    let snapped = anchor.saturating_add(blocks.saturating_mul(block));

    if snapped.abs_diff(estimate) <= tolerance {
        snapped
    } else {
        estimate
    }
}

/// Whether to seek to the requested start instead of decoding up to it.
///
/// Seeking avoids decoding data that is thrown away again, but it is only worth
/// it, and only frame-exact, under all of these conditions:
///
/// - The offset is more than one second. Below that the seek overhead is not
///   worth it, and decoding from the beginning while discarding samples is
///   simpler.
/// - Every timestamp tick maps to a whole number of audio frames, so that the
///   seek target and the landing can be expressed exactly.
/// - The container is not Matroska. Symphonia's Matroska accurate seek lands on
///   a cue point that can be well after the requested target, and the read has
///   to reopen the file and decode from the beginning whenever the landing turns
///   out to be unusable, which is slower than not seeking in the first place.
///   The landing is still validated for every container, so this only avoids the
///   wasted attempt.
fn should_seek(start_frame: usize, sample_rate: u32, tb: TimeBase, format_name: &str) -> bool {
    start_frame > sample_rate as usize
        && time_base_has_exact_frames(tb, sample_rate)
        && format_name != "matroska"
}

/// Seek so that `start_frame` can still be decoded, and report whether the
/// landing is usable.
///
/// The seek aims one second early to give codecs with inter-frame dependencies
/// time to warm up. Some format readers still land after the requested start
/// despite [`SeekMode::Accurate`]; such a landing cannot produce the full
/// requested range, and neither can a seek that failed outright. Both leave the
/// format reader at an unspecified position, so a `false` return means the caller
/// has to reopen the file.
fn seek_before(
    format: &mut dyn FormatReader,
    track_id: u32,
    start_frame: usize,
    sample_rate: u32,
    tb: TimeBase,
) -> bool {
    let target = start_frame.saturating_sub(sample_rate as usize) as u64;
    let ts = i64::try_from(frames_to_ts(target, tb, sample_rate)).unwrap_or(i64::MAX);

    let result = format.seek(
        SeekMode::Accurate,
        SeekTo::Timestamp {
            ts: Timestamp::new(ts),
            track_id,
        },
    );

    matches!(result, Ok(landing)
        if seek_landing_is_safe(landing.actual_ts.get(), start_frame as u64, tb, sample_rate))
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
    use super::*;

    // Only the tests reading the WAV fixtures use these
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    use audio_blocks::{AudioBlock, InterleavedView};
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    use std::time::Duration;

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    fn to_block<F: num::Float + 'static>(audio: &Audio<F>) -> InterleavedView<'_, F> {
        InterleavedView::from_slice(&audio.samples_interleaved, audio.num_channels)
    }

    /// Verify that the read audio data matches the expected sine wave values.
    /// The test file was generated by utils/generate_wav.py with these parameters:
    /// - 4 channels with frequencies: [440, 554.37, 659.25, 880] Hz
    /// - Sample rate: 48000 Hz
    /// - Duration: 1 second (48000 samples)
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    /// LAME records its encoder delay and padding in the Xing header, symphonia
    /// signals the delay frames with a negative PTS plus a start trim, and the
    /// MP3 decoder drops them. Frame 0 of the read is therefore the first
    /// playable frame, sample-aligned with the signal that was encoded.
    #[cfg(any(feature = "all-codecs", feature = "mp3"))]
    #[test]
    fn test_mp3_encoder_delay_is_not_part_of_the_timeline() {
        // The fixture encodes test_1ch.wav: one second of a 440 Hz sine at 48 kHz
        const SAMPLE_RATE: f64 = 48_000.0;
        const FREQUENCY: f64 = 440.0;
        const N_FRAMES: usize = 48_000;

        let mp3 = read::<f32>("test_data/test_mp3.mp3", ReadConfig::default()).unwrap();
        assert_eq!(mp3.sample_rate, SAMPLE_RATE as u32);
        assert_eq!(mp3.num_channels, 1);
        // Delay and padding are not part of the timeline, so the length is the
        // length of the encoded signal and not of the decoded frames
        assert_eq!(mp3.samples_interleaved.len(), N_FRAMES);

        let sine = |frame: usize| {
            (2.0 * std::f64::consts::PI * FREQUENCY * frame as f64 / SAMPLE_RATE).sin() as f32
        };
        // Compare in the middle of the file, away from the lossy codec's edges
        let error_at = |shift: i64| -> f32 {
            (20_000..21_000)
                .map(|frame| {
                    let shifted = (frame as i64 + shift) as usize;
                    (mp3.samples_interleaved[shifted] - sine(frame)).abs()
                })
                .fold(0.0, f32::max)
        };

        let aligned = error_at(0);
        assert!(aligned < 0.05, "MP3 decoded too inaccurately: {aligned}");
        // No shift reproduces the encoded signal better than no shift at all,
        // which only holds if the delay frames were trimmed exactly
        for shift in [-2, -1, 1, 2] {
            assert!(
                aligned < error_at(shift),
                "shift {shift} fits better ({}) than no shift ({aligned})",
                error_at(shift)
            );
        }
    }

    #[test]
    fn test_seek_is_only_worth_it_for_exact_seekable_containers() {
        let ms = TimeBase::try_new(1, 1_000).unwrap();

        // Below one second, decoding from the beginning is simpler
        assert!(!should_seek(48_000, 48_000, ms, "ogg"));
        assert!(should_seek(48_001, 48_000, ms, "ogg"));
        // 44.1 kHz has no whole number of frames per millisecond tick
        assert!(!should_seek(100_000, 44_100, ms, "ogg"));
        // Matroska lands on cue points, so the attempt is skipped for it
        assert!(!should_seek(100_000, 48_000, ms, "matroska"));
        assert!(should_seek(100_000, 48_000, ms, "wav"));
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
    }

    #[test]
    fn test_plan_is_resolved_against_the_given_specification() {
        let config = ReadConfig {
            start: Position::Time(std::time::Duration::from_millis(10)),
            stop: Position::Time(std::time::Duration::from_millis(20)),
            start_channel: Some(1),
            num_channels: Some(2),
            ..Default::default()
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

    #[test]
    fn test_only_the_selected_channels_of_the_selected_frames_are_appended() {
        // Three frames of four channels, one digit per channel
        let interleaved: Vec<f64> = (0..12).map(f64::from).collect();
        let layout = |start, count| Layout {
            total: 4,
            start,
            count,
        };

        let mut all = Vec::new();
        append_selected::<f32>(&mut all, &interleaved, layout(0, 4), 0..3);
        assert_eq!(all, (0..12).map(|s| s as f32).collect::<Vec<_>>());

        // Appending keeps what is already in the output
        append_selected::<f32>(&mut all, &interleaved, layout(1, 2), 1..3);
        assert_eq!(all[12..], [5.0, 6.0, 9.0, 10.0]);

        // The last channels of the last frame
        let mut tail = Vec::new();
        append_selected::<f32>(&mut tail, &interleaved, layout(2, 2), 2..3);
        assert_eq!(tail, [10.0, 11.0]);

        // An empty frame range appends nothing
        let mut none = Vec::new();
        append_selected::<f32>(&mut none, &interleaved, layout(0, 4), 1..1);
        assert!(none.is_empty());
    }

    #[test]
    fn test_frames_per_tick_bounds_the_timestamp_error() {
        let tb = |numer, denom| TimeBase::try_new(numer, denom).unwrap();

        // A millisecond tick spans 44.1 frames at 44.1 kHz, so a timestamp can be
        // up to 45 frames away from the frame it refers to
        assert_eq!(frames_per_tick(tb(1, 1000), 44_100), 45);
        // The same tick is exact at 48 kHz
        assert_eq!(frames_per_tick(tb(1, 1000), 48_000), 48);
        // A time base of one tick per frame cannot be off at all
        assert_eq!(frames_per_tick(tb(1, 44_100), 44_100), 1);
    }

    #[test]
    fn test_snap_to_grid_recovers_a_quantized_position() {
        // One FLAC block after the anchor, as a millisecond timestamp at 44.1 kHz
        // reports it: 22 frames early
        assert_eq!(snap_to_grid(4586, 0, 4608, 45), 4608);
        // Several blocks after a non-zero anchor, quantized late
        assert_eq!(
            snap_to_grid(10_000 + 3 * 4608 + 30, 10_000, 4608, 45),
            23_824
        );
        // An exact position is already on the grid and stays put
        assert_eq!(snap_to_grid(9216, 0, 4608, 1), 9216);

        // At least one packet was lost, so the anchor itself is never the answer
        assert_eq!(snap_to_grid(20, 0, 4608, 4608), 4608);

        // An estimate too far off any grid point is left alone: something other
        // than timestamp quantization moved it, and rounding would introduce an
        // error of up to half a block instead of removing one of a few frames
        assert_eq!(snap_to_grid(7000, 0, 4608, 45), 7000);
        // The same estimate against a spacing it does fit
        assert_eq!(snap_to_grid(7000, 0, 3500, 45), 7000);

        // An estimate before the anchor is not on the grid ahead of it
        assert_eq!(snap_to_grid(500, 1000, 4608, 45), 500);
    }

    #[test]
    fn test_expected_frames_is_bounded_by_the_file_length() {
        let bounded = Plan::resolve(
            48_000,
            2,
            &ReadConfig {
                start: Position::Frame(100),
                stop: Position::Frame(600),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bounded.expected_frames(None), Some(500));
        // A file that ends before the stop position bounds the read
        assert_eq!(bounded.expected_frames(Some(300)), Some(200));
        assert_eq!(bounded.expected_frames(Some(50)), Some(0));

        let unbounded = Plan::resolve(48_000, 2, &ReadConfig::default()).unwrap();
        assert_eq!(unbounded.expected_frames(None), None);
        assert_eq!(unbounded.expected_frames(Some(300)), Some(300));
    }

    #[test]
    fn test_declared_plan_is_used_when_nothing_was_decoded() {
        let track = |channels| TrackInfo {
            id: 0,
            sample_rate: 48_000,
            channels,
            time_base: None,
            num_frames: None,
        };

        let plan = declared_plan(&track(Some(2)), &ReadConfig::default()).unwrap();
        assert_eq!(plan.sample_rate, 48_000);
        assert_eq!(plan.layout.count, 2);

        // An invalid selection is rejected without decoded audio too
        let too_many = ReadConfig {
            num_channels: Some(3),
            ..Default::default()
        };
        assert!(matches!(
            declared_plan(&track(Some(2)), &too_many),
            Err(ReadError::InvalidChannelRange { total: 2, .. })
        ));

        // Neither decoded nor declared, so the channel count is unknown
        assert!(matches!(
            declared_plan(&track(None), &ReadConfig::default()),
            Err(ReadError::NoChannels)
        ));
    }

    #[test]
    fn test_gap_fill_is_bounded() {
        // A hole inside a known length is filled completely
        assert_eq!(fill_frames(100, 900, 2, Some(2_000)), 100);
        // ... but never beyond the frames the read can still produce
        assert_eq!(fill_frames(100, 1_950, 2, Some(2_000)), 50);
        assert_eq!(fill_frames(100, 2_000, 2, Some(2_000)), 0);
        // Without a known length, a bogus timestamp is capped by the same budget
        // as the preallocation
        assert_eq!(fill_frames(100, 0, 2, None), 100);
        assert_eq!(fill_frames(u64::MAX, 0, 2, None), MAX_PREALLOC_SAMPLES / 2);
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
        // The ranges are frame ranges, the buffers hold interleaved samples
        let channels = usize::from(full.num_channels);

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

            let expected = &full.samples_interleaved[range.start * channels..range.end * channels];
            assert_eq!(
                selected.num_channels, full.num_channels,
                "{path}: range {range:?} changed the channel count"
            );
            assert!(
                selected.samples_interleaved.len() <= range.len() * channels,
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

    /// The Matroska track metadata declares 22.05 kHz while the FLAC stream info
    /// declares 44.1 kHz. Symphonia's Matroska demuxer reports the container
    /// value, so trusting it labels the returned audio with the wrong rate and
    /// resolves time positions and the resampling ratio against it.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_sample_rate_uses_decoded_stream_info() {
        const PATH: &str = "test_data/test_declared_rate_mismatch.mka";

        // Assert the fixture still contains the intended metadata disagreement.
        let format = open_format(Path::new(PATH)).unwrap();
        let declared_rate = format
            .default_track(TrackType::Audio)
            .and_then(|track| track.codec_params.as_ref())
            .and_then(CodecParameters::audio)
            .and_then(|params| params.sample_rate);
        assert_eq!(declared_rate, Some(22_050));

        let full = read::<f32>(PATH, ReadConfig::default()).unwrap();
        assert_eq!(full.sample_rate, 44_100);
        assert_eq!(full.num_channels, 1);
        // 20 ms of 44.1 kHz audio
        assert_eq!(full.samples_interleaved.len(), 882);

        // A time position must be resolved against the decoded rate, otherwise
        // 10 ms yields the 220 frames that the declared rate implies.
        let selected = read::<f32>(
            PATH,
            ReadConfig {
                stop: Position::Time(std::time::Duration::from_millis(10)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(selected.sample_rate, 44_100);
        assert_eq!(selected.samples_interleaved.len(), 441);
        assert_eq!(
            selected.samples_interleaved,
            full.samples_interleaved[..441]
        );

        // Resampling must start from the decoded rate, so asking for the rate
        // the file really has must not change its length.
        let resampled = read::<f32>(
            PATH,
            ReadConfig {
                sample_rate: Some(44_100),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(resampled.sample_rate, 44_100);
        assert_eq!(
            resampled.samples_interleaved.len(),
            full.samples_interleaved.len()
        );
    }

    /// A packet the decoder rejects is discarded and the position is recovered
    /// from the next timestamp. The frames of the discarded packet are lost, so
    /// they have to be filled with silence: without the fill every later frame
    /// moves earlier in the output and the read silently returns audio that no
    /// longer lines up with the frame positions it was asked for.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_discarded_packet_keeps_later_frames_aligned() {
        // At 48 kHz a millisecond timestamp is an exact frame boundary, so the
        // position after the hole comes straight from the timestamp. At 44.1 kHz it
        // is not, and the exact position is only recoverable from the grid that the
        // constant FLAC blocksize puts the packets on.
        assert_discarded_packet_leaves_one_silent_hole("test_data/test_flac_48k.mka");
        assert_discarded_packet_leaves_one_silent_hole("test_data/test_flac.mka");
    }

    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    fn assert_discarded_packet_leaves_one_silent_hole(source_path: &str) {
        let intact = read::<f32>(source_path, ReadConfig::default())
            .unwrap()
            .samples_interleaved;
        let source = std::fs::read(source_path).unwrap();

        // A FLAC frame header is protected by a CRC-8, so flipping a bit in it
        // makes the decoder reject that one packet while all the others stay
        // decodable. The 14-bit sync word also occurs inside audio data, so try
        // the candidates until one really loses a packet.
        let syncs = (0..source.len() - 1)
            .filter(|&i| source[i] == 0xFF && source[i + 1] & 0xFC == 0xF8)
            .collect::<Vec<_>>();
        assert!(!syncs.is_empty(), "fixture: no FLAC frame sync word found");

        let path = std::env::temp_dir().join("audio-file-discarded-packet.mka");
        for sync in syncs {
            let mut damaged_source = source.clone();
            damaged_source[sync + 3] ^= 0x0F;
            std::fs::write(&path, &damaged_source).unwrap();

            let Ok(damaged) = read::<f32>(&path, ReadConfig::default()) else {
                continue;
            };
            let damaged = damaged.samples_interleaved;
            let hole = silent_hole(&damaged, &intact);

            // A discarded packet either leaves a silent hole, or, without the
            // fill, a shorter read. Anything else only corrupted the audio of a
            // packet that still decoded, which is not the case under test.
            if damaged.len() == intact.len() && hole.is_none() {
                continue;
            }

            assert_eq!(
                damaged.len(),
                intact.len(),
                "the discarded packet shortened the read instead of leaving a hole"
            );
            let (gap_start, gap_end) = hole.expect("the hole must be silent");
            assert!(
                gap_end - gap_start < intact.len() / 4,
                "{source_path}: more than one packet was discarded: {gap_start}..{gap_end}"
            );
            // Everything around the hole is still at its own position, frame for
            // frame, which only holds if the recovered position was exact.
            assert_eq!(damaged[..gap_start], intact[..gap_start], "{source_path}");
            assert_eq!(damaged[gap_end..], intact[gap_end..], "{source_path}");

            std::fs::remove_file(&path).unwrap();
            return;
        }

        panic!("{source_path}: no corrupted frame header made the decoder discard a packet");
    }

    /// The range `a` and `b` disagree over, if `a` is silent across all of it.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    fn silent_hole(a: &[f32], b: &[f32]) -> Option<(usize, usize)> {
        let start = a.iter().zip(b).position(|(x, y)| x != y)?;
        let trailing = a
            .iter()
            .rev()
            .zip(b.iter().rev())
            .position(|(x, y)| x != y)?;
        let end = a.len() - trailing;
        a[start..end]
            .iter()
            .all(|&s| s == 0.0)
            .then_some((start, end))
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

    /// A chained stream may also change its sample rate, which cannot be
    /// represented in one buffer either and would play back at the wrong speed.
    #[cfg(all(
        any(feature = "all-codecs", feature = "ogg"),
        any(feature = "all-codecs", feature = "vorbis")
    ))]
    #[test]
    fn test_mid_stream_sample_rate_change_is_rejected() {
        let error = read::<f32>(
            "test_data/test_sample_rate_change.ogg",
            ReadConfig::default(),
        )
        .expect_err("the chained stream changes from 48 kHz to 44.1 kHz");

        assert!(
            matches!(
                error,
                ReadError::SampleRateChanged {
                    expected: 48_000,
                    found: 44_100
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

    /// At 48 kHz every Matroska millisecond tick is a whole number of frames, so
    /// the seek decision is the only thing keeping this container off the seek
    /// path. The ranges past the one-second threshold therefore cover the
    /// anchoring for an exact time base as well.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_flac_matroska_48k_frame_ranges() {
        assert_ranges_match_full_decode(
            "test_data/test_flac_48k.mka",
            &[0..100, 48_001..48_101, 100_000..100_100, 143_900..144_000],
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
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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

    /// `read_block` is the same read, wrapped in an interleaved audio block.
    #[cfg(all(
        feature = "audio-blocks",
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
    #[test]
    fn test_read_block_matches_read() {
        let config = || ReadConfig {
            start: Position::Frame(1_000),
            stop: Position::Frame(1_500),
            start_channel: Some(1),
            num_channels: Some(2),
            ..Default::default()
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
    #[cfg(all(
        any(feature = "all-codecs", feature = "wav"),
        any(feature = "all-codecs", feature = "pcm")
    ))]
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
