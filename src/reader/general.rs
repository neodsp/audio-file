//! The general decoding path, through Symphonia, for every format the wav
//! fast path in [`super`] does not claim.
//!
//! Unlike wav, a compressed format has no formula for where frame `n` lives:
//! the position of a decoded packet has to be recovered from its timestamp,
//! decoders need warm-up packets before they emit real audio, and a seek
//! landing has to be verified rather than trusted. Everything below exists to
//! turn that into the same frame-exact, channel-exact read the wav path gets
//! from indexing into a byte array.
//!
//! It repairs nothing: a packet the decoder rejects ends the read with an error
//! rather than being skipped over.

use std::fs::File;
use std::path::Path;

use num_traits::Float;
use symphonia::core::audio::Channels;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

use super::{Decoded, Layout, MAX_PREALLOC_SAMPLES, Plan, ReadConfig, ReadError};

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

impl Plan {
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

/// The general decoding path, for every format the wav fast path in [`super`]
/// does not claim.
pub(super) fn decode_with_symphonia<F: Float>(
    path: &Path,
    config: &ReadConfig,
) -> Result<Decoded<F>, ReadError> {
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
    // First frame that has not been written to the output yet. Set to the start
    // frame as soon as the plan is resolved. A packet that begins beyond it means
    // the stream skipped frames the output has no way to leave out.
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
                // the base if a later reset requires re-anchoring from this new
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
            // The audio specification of the decoded audio may change after a
            // reset, which is picked up from the next packet.
            Err(Error::ResetRequired) => {
                decoder.reset();
                position = None;
                continue;
            }
            // A packet the decoder rejects means the file is damaged, truncated,
            // or in an encoding this build cannot decode. Its frames are simply
            // gone: their number is not even known, so nothing can take their
            // place, and continuing would either shift the rest of the read off
            // its positions or present invented silence as audio. Neither is
            // something the caller could detect, so the read fails instead.
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
                        unverified_landing = true;
                    } else {
                        // A failed seek is not guaranteed to leave every format
                        // reader at its original position. Reopening is also the
                        // only reliable way to undo a successful seek that
                        // overshot the requested start, and it decodes this
                        // packet again from a known position.
                        format = open_format(path)?;
                        (track, decoder) = select_track(&*format, &dec_opts)?;
                        position = Some(0);
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
                Some(tb) => stream_base.saturating_add(timestamp_to_frame(
                    packet.pts.get(),
                    packet.trim_start.get(),
                    tb,
                    plan.sample_rate,
                )),
                None => stream_base,
            },
        };

        // A format reader can report a landing at or before the requested start and
        // still deliver its first packet after it. The frames in between were never
        // lost, so the read would quietly begin late and come up short rather than
        // being able to report anything. Decoding from the beginning always reaches
        // the requested start, which is what not seeking would have cost anyway.
        if std::mem::take(&mut unverified_landing) && packet_start > plan.start_frame as u64 {
            format = open_format(path)?;
            (track, decoder) = select_track(&*format, &dec_opts)?;
            position = Some(0);
            continue;
        }

        // Frames the output is still waiting for that this packet begins after are
        // ones the stream did not deliver. Skipping them would move every later
        // frame off the position it was asked for, and appending nothing in their
        // place would return a buffer that is short without saying so. A seek
        // landing that starts late was already caught above, so a real
        // discontinuity in the file is what is left to get here.
        let expected_until = match plan.end_frame {
            Some(end) => packet_start.min(end as u64),
            None => packet_start,
        };
        if expected_until > copied_until {
            return Err(ReadError::MissingFrames {
                start: copied_until,
                end: expected_until,
            });
        }

        let packet_end = packet_start.saturating_add(packet_frames as u64);

        // Intersect the packet with the requested frame range
        let copy_start = packet_start.max(plan.start_frame as u64).max(copied_until);
        let copy_end = match plan.end_frame {
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

/// Frame index that a signed timestamp and subsequent frame trim refer to.
fn timestamp_to_frame(ts: i64, trim_start: u64, tb: TimeBase, sample_rate: u32) -> u64 {
    let frames = i128::from(ts) * i128::from(tb.numer.get()) * i128::from(sample_rate)
        / i128::from(tb.denom.get())
        + i128::from(trim_start);
    u64::try_from(frames).unwrap_or(if frames < 0 { 0 } else { u64::MAX })
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let mp3 =
            crate::reader::read::<f32>("test_data/test_mp3.mp3", ReadConfig::default()).unwrap();
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

    /// `should_seek` recognizes Matroska by the `short_name` symphonia reports,
    /// which a symphonia update could rename. A rename would only cost the seek
    /// skip, since the landing validation reopens the file anyway, so this is a
    /// canary, not a guard against a bug.
    #[cfg(any(feature = "all-codecs", feature = "mkv"))]
    #[test]
    fn test_matroska_is_still_named_matroska() {
        let format = open_format(Path::new("test_data/test_flac.mka")).unwrap();
        assert_eq!(format.format_info().short_name, "matroska");
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
    fn test_expected_frames_is_bounded_by_the_file_length() {
        let bounded = Plan::resolve(
            48_000,
            2,
            &ReadConfig {
                start: crate::reader::Position::Frame(100),
                stop: crate::reader::Position::Frame(600),
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
        let full = crate::reader::read::<f32>(path, ReadConfig::default()).unwrap();
        // The ranges are frame ranges, the buffers hold interleaved samples
        let channels = usize::from(full.num_channels);

        for range in ranges {
            let selected = crate::reader::read::<f32>(
                path,
                ReadConfig {
                    start: crate::reader::Position::Frame(range.start),
                    stop: crate::reader::Position::Frame(range.end),
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

        let audio = crate::reader::read::<f32>(PATH, ReadConfig::default()).unwrap();

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

        let full = crate::reader::read::<f32>(PATH, ReadConfig::default()).unwrap();
        assert_eq!(full.num_channels, 2);
        assert!(!full.samples_interleaved.is_empty());

        // This selection is invalid against the declared mono layout but valid
        // against the authoritative decoded stereo layout.
        let selected = crate::reader::read::<f32>(
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

        let full = crate::reader::read::<f32>(PATH, ReadConfig::default()).unwrap();
        assert_eq!(full.sample_rate, 44_100);
        assert_eq!(full.num_channels, 1);
        // 20 ms of 44.1 kHz audio
        assert_eq!(full.samples_interleaved.len(), 882);

        // A time position must be resolved against the decoded rate, otherwise
        // 10 ms yields the 220 frames that the declared rate implies.
        let selected = crate::reader::read::<f32>(
            PATH,
            ReadConfig {
                stop: crate::reader::Position::Time(std::time::Duration::from_millis(10)),
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
        let resampled = crate::reader::read::<f32>(
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

    /// A packet the decoder rejects fails the whole read. The alternative would
    /// be to drop it and carry on, which returns a buffer that is either shorter
    /// than the file or padded with silence the file never contained, neither of
    /// which the caller can tell apart from real audio.
    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    #[test]
    fn test_damaged_packet_is_a_read_error() {
        assert_damaged_packet_is_rejected("test_data/test_flac_48k.mka");
        assert_damaged_packet_is_rejected("test_data/test_flac.mka");
    }

    #[cfg(all(
        any(feature = "all-codecs", feature = "mkv"),
        any(feature = "all-codecs", feature = "flac")
    ))]
    fn assert_damaged_packet_is_rejected(source_path: &str) {
        let intact = crate::reader::read::<f32>(source_path, ReadConfig::default())
            .unwrap()
            .samples_interleaved;
        let source = std::fs::read(source_path).unwrap();

        // A FLAC frame header is protected by a CRC-8, so flipping a bit in it
        // makes the decoder reject that one packet while all the others stay
        // decodable. The 14-bit sync word also occurs inside audio data, so try
        // the candidates until one really damages a packet header.
        let syncs = (0..source.len() - 1)
            .filter(|&i| source[i] == 0xFF && source[i + 1] & 0xFC == 0xF8)
            .collect::<Vec<_>>();
        assert!(!syncs.is_empty(), "fixture: no FLAC frame sync word found");

        let path = crate::tmp_path("damaged-packet.mka");
        for sync in syncs {
            let mut damaged_source = source.clone();
            damaged_source[sync + 3] ^= 0x0F;
            std::fs::write(&path, &damaged_source).unwrap();

            match crate::reader::read::<f32>(&path, ReadConfig::default()) {
                // The flip landed inside a packet that still decodes, so the
                // audio differs but no frames were lost. Not the case under test,
                // and the length still has to be untouched: a read that comes up
                // short without saying so is exactly what this guards against.
                Ok(damaged) => assert_eq!(
                    damaged.samples_interleaved.len(),
                    intact.len(),
                    "{source_path}: the read lost frames without reporting it"
                ),
                Err(error) => {
                    assert!(
                        matches!(error, ReadError::Decode(_)),
                        "{source_path}: {error:?}"
                    );
                    assert!(!error.to_string().is_empty());
                    std::fs::remove_file(&path).unwrap();
                    return;
                }
            }
        }

        panic!("{source_path}: no corrupted frame header made the decoder reject a packet");
    }

    /// Chained Ogg streams may replace the decoder and decoded layout. A real
    /// channel-count change cannot be represented in one interleaved output.
    #[cfg(all(
        any(feature = "all-codecs", feature = "ogg"),
        any(feature = "all-codecs", feature = "vorbis")
    ))]
    #[test]
    fn test_mid_stream_channel_count_change_is_rejected() {
        let error = crate::reader::read::<f32>(
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
        let error = crate::reader::read::<f32>(
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

    /// The WAV fast path and the Symphonia path must agree exactly, not
    /// approximately: both normalize an integer sample by dividing by
    /// `2^(bits-1)`, so every value either matches bit for bit or one of the
    /// two is reading the file wrongly.
    ///
    /// This is the test that pins the sample scaling, the channel
    /// interleaving and the frame counting all at once. Any of them could be
    /// off by a factor of two, a channel or a frame and still look plausible
    /// on its own.
    #[cfg(any(feature = "all-codecs", feature = "wav-compressed"))]
    #[test]
    fn test_native_wav_matches_symphonia_exactly() {
        use crate::writer::{SampleFormat, WriteConfig, write};

        const FORMATS: [SampleFormat; 4] = [
            SampleFormat::Int8,
            SampleFormat::Int16,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ];

        // Values that land on the awkward parts of every scale: full scale
        // both ways, silence, and a spread in between.
        let source: Vec<f32> = (0..600)
            .map(|i| ((i as f32 / 300.0) - 1.0).clamp(-1.0, 1.0))
            .collect();

        for format in FORMATS {
            // Stays at or below 18 channels, since above that Symphonia
            // refuses the extensible layout and there is nothing to compare
            // against. That ceiling is exactly what the fast path lifts.
            for num_channels in [1u16, 2, 3, 6, 18] {
                let frames = source.len() / usize::from(num_channels);
                let samples = &source[..frames * usize::from(num_channels)];

                let path = crate::tmp_path(&format!("diff-{format:?}-{num_channels}ch.wav"));
                write(
                    &path,
                    samples,
                    num_channels,
                    48_000,
                    WriteConfig {
                        sample_format: format,
                    },
                )
                .unwrap();

                for config in [
                    ReadConfig::default(),
                    ReadConfig {
                        start: crate::reader::Position::Frame(7),
                        stop: crate::reader::Position::Frame(23),
                        ..Default::default()
                    },
                    ReadConfig {
                        start_channel: Some(usize::from(num_channels) - 1),
                        ..Default::default()
                    },
                    ReadConfig {
                        start: crate::reader::Position::Frame(3),
                        num_channels: Some(1),
                        ..Default::default()
                    },
                ] {
                    let native = crate::reader::try_native_wav::<f64>(&path, &config)
                        .unwrap()
                        .expect("a PCM wav file must take the fast path");
                    let symphonia = decode_with_symphonia::<f64>(&path, &config).unwrap();

                    let label = format!("{format:?} {num_channels}ch");
                    assert_eq!(native.sample_rate, symphonia.sample_rate, "{label}");
                    assert_eq!(native.num_channels, symphonia.num_channels, "{label}");
                    assert_eq!(
                        native.samples, symphonia.samples,
                        "{label}: sample values must be bit-identical"
                    );
                }

                std::fs::remove_file(&path).unwrap();
            }
        }
    }

    /// The same comparison against the checked-in fixtures, which were not
    /// produced by this crate's encoder and so exercise a `fmt ` chunk this
    /// crate never writes itself. It also pins that these files really do
    /// take the fast path: if the native decoder ever started refusing them,
    /// every other WAV test would keep passing via the fallback and say
    /// nothing.
    #[cfg(any(feature = "all-codecs", feature = "wav-compressed"))]
    #[test]
    fn test_native_wav_matches_symphonia_on_the_fixtures() {
        for file in ["test_data/test_1ch.wav", "test_data/test_4ch.wav"] {
            let path = std::path::Path::new(file);

            for config in [
                ReadConfig::default(),
                ReadConfig {
                    start: crate::reader::Position::Frame(1_000),
                    stop: crate::reader::Position::Frame(1_100),
                    ..Default::default()
                },
                ReadConfig {
                    stop: crate::reader::Position::Time(std::time::Duration::from_secs_f32(0.25)),
                    ..Default::default()
                },
            ] {
                let native = crate::reader::try_native_wav::<f64>(path, &config)
                    .unwrap()
                    .unwrap_or_else(|| panic!("{file} must take the fast path"));
                let symphonia = decode_with_symphonia::<f64>(path, &config).unwrap();

                assert_eq!(native.sample_rate, symphonia.sample_rate, "{file}");
                assert_eq!(native.num_channels, symphonia.num_channels, "{file}");
                assert_eq!(
                    native.samples.len(),
                    symphonia.samples.len(),
                    "{file}: frame counts must agree"
                );
                assert_eq!(native.samples, symphonia.samples, "{file}");
            }
        }
    }

    /// A `WAVEFORMATEXTENSIBLE` file whose `wValidBitsPerSample` is below its
    /// `wBitsPerSample` is read according to the container width, because the
    /// valid bits are left-justified inside it. Reading such a file as though
    /// the value were right-justified in the low bits takes the wrong bits
    /// and comes out 48 dB quiet, which is the kind of error that still looks
    /// like audio. Symphonia decides this the same way, so it can arbitrate.
    #[cfg(any(feature = "all-codecs", feature = "wav-compressed"))]
    #[test]
    fn test_extensible_valid_bits_below_container_matches_symphonia() {
        use crate::writer::{SampleFormat, WriteConfig, write};

        let samples: Vec<f32> = (0..64).map(|i| (i as f32 / 32.0) - 1.0).collect();
        let path = crate::tmp_path("valid-bits-24-in-32.wav");
        write(
            &path,
            &samples,
            4,
            48_000,
            WriteConfig {
                sample_format: SampleFormat::Int32,
            },
        )
        .unwrap();

        // Patch wValidBitsPerSample from 32 down to 24, leaving
        // wBitsPerSample at 32. The sample bytes are untouched, so the
        // correct reading is unchanged.
        let mut bytes = std::fs::read(&path).unwrap();
        let fmt_body = 12 + 8;
        let valid_bits_offset = fmt_body + 18;
        assert_eq!(
            u16::from_le_bytes(
                bytes[valid_bits_offset..valid_bits_offset + 2]
                    .try_into()
                    .unwrap()
            ),
            32,
            "the encoder should have written a full-width wValidBitsPerSample"
        );
        bytes[valid_bits_offset..valid_bits_offset + 2].copy_from_slice(&24u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let native = crate::reader::try_native_wav::<f64>(&path, &ReadConfig::default())
            .unwrap()
            .expect("still a PCM wav file");
        let symphonia = decode_with_symphonia::<f64>(&path, &ReadConfig::default()).unwrap();

        assert_eq!(native.samples, symphonia.samples);
        // And the values are still the ones that were written, rather than
        // the low 24 bits of them.
        approx::assert_abs_diff_eq!(
            native
                .samples
                .iter()
                .map(|&s| s as f32)
                .collect::<Vec<_>>()
                .as_slice(),
            samples.as_slice(),
            epsilon = 1e-6
        );

        std::fs::remove_file(&path).unwrap();
    }
}
