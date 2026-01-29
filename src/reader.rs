use std::fs::File;
use std::path::Path;

use num::Float;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use thiserror::Error;

use crate::resample::resample;

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
pub enum AudioReadError {
    #[error("could not read file")]
    FileError(#[from] std::io::Error),
    #[error("could not decode audio")]
    EncodingError(#[from] symphonia::core::errors::Error),
    #[error("could not find track in file")]
    NoTrack,
    #[error("could not find sample rate in file")]
    NoSampleRate,
    #[error("end frame {0} is larger than start frame {1}")]
    EndFrameLargerThanStartFrame(usize, usize),
    #[error("start channel {0} invalid, audio file has only {1} channels")]
    InvalidStartChannel(usize, usize),
    #[error("invalid number of channels to extract: {0}")]
    InvalidNumChannels(usize),
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
pub struct AudioReadConfig {
    /// Where to start reading audio (time or frame-based)
    pub start: Position,
    /// Where to stop reading audio (time or frame-based)
    pub stop: Position,
    /// Starting channel to extract (0-indexed). None means start from channel 0.
    pub start_channel: Option<usize>,
    /// Number of channels to extract. None means extract all remaining channels.
    pub num_channels: Option<usize>,
    /// If specified the audio will be resampled to the given sample rate
    pub sample_rate: Option<u32>,
}

pub fn audio_read<F: Float + rubato::Sample>(
    path: impl AsRef<Path>,
    config: AudioReadConfig,
) -> Result<Audio<F>, AudioReadError> {
    let src = File::open(path.as_ref())?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.as_ref().extension()
        && let Some(ext_str) = ext.to_str()
    {
        hint.with_extension(ext_str);
    }

    let meta_opts: MetadataOptions = Default::default();
    let fmt_opts: FormatOptions = Default::default();

    let probed = symphonia::default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts)?;

    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(AudioReadError::NoTrack)?;

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or(AudioReadError::NoSampleRate)?;

    let track_id = track.id;

    // Clone codec params before the mutable borrow
    let codec_params = track.codec_params.clone();
    let time_base = track.codec_params.time_base;

    // Convert start/stop positions to frame numbers
    let start_frame = match config.start {
        Position::Default => 0,
        Position::Time(duration) => {
            let secs = duration.as_secs_f64();
            (secs * sample_rate as f64) as usize
        }
        Position::Frame(frame) => frame,
    };

    let end_frame: Option<usize> = match config.stop {
        Position::Default => None,
        Position::Time(duration) => {
            let secs = duration.as_secs_f64();
            Some((secs * sample_rate as f64) as usize)
        }
        Position::Frame(frame) => Some(frame),
    };

    if let Some(end_frame) = end_frame
        && start_frame > end_frame
    {
        return Err(AudioReadError::EndFrameLargerThanStartFrame(
            end_frame,
            start_frame,
        ));
    }

    // If start_frame is large (more than 1 second), use seeking to avoid decoding everything
    if start_frame > sample_rate as usize
        && let Some(tb) = time_base
    {
        // Seek to 90% of the target to account for keyframe positioning
        let seek_sample = (start_frame as f64 * 0.9) as u64;
        let seek_ts = (seek_sample * tb.denom as u64) / (sample_rate as u64);

        // Try to seek, but don't fail if seeking doesn't work
        let _ = format.seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: seek_ts,
                track_id,
            },
        );
    }

    let dec_opts: DecoderOptions = Default::default();
    let mut decoder = symphonia::default::get_codecs().make(&codec_params, &dec_opts)?;

    let mut sample_buf = None;
    let mut samples = Vec::new();
    let mut num_channels = 0usize;
    let start_channel = config.start_channel;

    // We'll track exact position by counting samples as we decode
    let mut current_sample: Option<u64> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(err) => return Err(err.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet)?;

        // Get the timestamp of this packet to know our position
        if current_sample.is_none() {
            let ts = packet.ts();
            if let Some(tb) = time_base {
                // Convert timestamp to sample position
                current_sample = Some((ts * sample_rate as u64) / tb.denom as u64);
            } else {
                current_sample = Some(0);
            }
        }

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            let duration = decoded.capacity() as u64;
            sample_buf = Some(SampleBuffer::<f32>::new(duration, spec));

            // Get the number of channels from the spec
            num_channels = spec.channels.count();

            // Validate channel range
            let ch_start = start_channel.unwrap_or(0);
            let ch_count = config.num_channels.unwrap_or(num_channels - ch_start);

            if ch_start >= num_channels {
                return Err(AudioReadError::InvalidStartChannel(ch_start, num_channels));
            }
            if ch_count == 0 {
                return Err(AudioReadError::InvalidNumChannels(0));
            }
            if ch_start + ch_count > num_channels {
                return Err(AudioReadError::InvalidNumChannels(ch_count));
            }
        }

        if let Some(buf) = &mut sample_buf {
            buf.copy_interleaved_ref(decoded);
            let packet_samples = buf.samples();

            let mut pos = current_sample.unwrap_or(0);

            // Determine channel range to extract
            let ch_start = start_channel.unwrap_or(0);
            let ch_count = config.num_channels.unwrap_or(num_channels - ch_start);
            let ch_end = ch_start + ch_count;

            // Calculate frames using the ORIGINAL channel count from the file
            let frames = packet_samples.len() / num_channels;

            // Process all frames, extracting only the requested channel range
            for frame_idx in 0..frames {
                // Check if we've reached the end frame
                if let Some(end) = end_frame
                    && pos >= end as u64
                {
                    return Ok(Audio {
                        samples_interleaved: samples,
                        sample_rate,
                        num_channels: ch_count as u16,
                    });
                }

                // Start collecting samples once we reach start_frame
                if pos >= start_frame as u64 {
                    // Extract the selected channel range from this frame
                    // When ch_start=0 and ch_count=num_channels, this extracts all channels
                    for ch in ch_start..ch_end {
                        let sample_idx = frame_idx * num_channels + ch;
                        samples.push(F::from(packet_samples[sample_idx]).unwrap());
                    }
                }

                pos += 1;
            }

            // Update our position tracker
            current_sample = Some(pos);
        }
    }

    let samples = if let Some(sr_out) = config.sample_rate {
        resample(&samples, num_channels, sample_rate, sr_out).unwrap()
    } else {
        samples
    };

    let ch_start = start_channel.unwrap_or(0);
    let ch_count = config.num_channels.unwrap_or(num_channels - ch_start);

    Ok(Audio {
        samples_interleaved: samples,
        sample_rate,
        num_channels: ch_count as u16,
    })
}

#[cfg(feature = "audio-blocks")]
pub fn audio_read_block<F: num::Float + 'static + rubato::Sample>(
    path: impl AsRef<Path>,
    config: AudioReadConfig,
) -> Result<(audio_blocks::AudioBlockInterleaved<F>, u32), AudioReadError> {
    let audio = audio_read(path, config)?;
    Ok((
        audio_blocks::AudioBlockInterleaved::from_slice(&audio.samples, audio.num_channels),
        audio.sample_rate,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use audio_blocks::{AudioBlock, AudioBlockInterleavedView};

    use super::*;

    fn to_block<F: num::Float + 'static>(audio: &Audio<F>) -> AudioBlockInterleavedView<'_, F> {
        AudioBlockInterleavedView::from_slice(&audio.samples_interleaved, audio.num_channels)
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

        let audio =
            audio_read::<f32>("test_data/test_4ch.wav", AudioReadConfig::default()).unwrap();
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
        let audio = audio_read::<f32>(
            "test_data/test_4ch.wav",
            AudioReadConfig {
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
        let audio1 =
            audio_read::<f32>("test_data/test_1ch.wav", AudioReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 1);

        let audio2 = audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
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
        let audio1 =
            audio_read::<f32>("test_data/test_1ch.wav", AudioReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 1);

        let audio2 = audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
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
        let audio1 =
            audio_read::<f32>("test_data/test_4ch.wav", AudioReadConfig::default()).unwrap();
        let block1 = to_block(&audio1);
        assert_eq!(audio1.sample_rate, 48000);
        assert_eq!(block1.num_frames(), 48000);
        assert_eq!(block1.num_channels(), 4);

        let audio2 = audio_read::<f32>(
            "test_data/test_4ch.wav",
            AudioReadConfig {
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
        match audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
                start: Position::Frame(100),
                stop: Position::Frame(99),
                ..Default::default()
            },
        ) {
            Err(AudioReadError::EndFrameLargerThanStartFrame(_, _)) => (),
            _ => panic!(),
        }

        match audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
                start: Position::Time(Duration::from_secs_f32(0.6)),
                stop: Position::Time(Duration::from_secs_f32(0.5)),
                ..Default::default()
            },
        ) {
            Err(AudioReadError::EndFrameLargerThanStartFrame(_, _)) => (),
            _ => panic!(),
        }

        match audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
                start_channel: Some(1),
                ..Default::default()
            },
        ) {
            Err(AudioReadError::InvalidStartChannel(_, _)) => (),
            _ => panic!(),
        }

        match audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
                num_channels: Some(0),
                ..Default::default()
            },
        ) {
            Err(AudioReadError::InvalidNumChannels(0)) => (),
            _ => panic!(),
        }

        match audio_read::<f32>(
            "test_data/test_1ch.wav",
            AudioReadConfig {
                num_channels: Some(2),
                ..Default::default()
            },
        ) {
            Err(AudioReadError::InvalidNumChannels(2)) => (),
            _ => panic!(),
        }
    }

    #[test]
    fn test_resample_preserves_frequency() {
        const FREQUENCIES: [f64; 4] = [440.0, 554.37, 659.25, 880.0];
        let sr_out: u32 = 22050;

        // Read and resample in one step
        let audio = audio_read::<f32>(
            "test_data/test_4ch.wav",
            AudioReadConfig {
                sample_rate: Some(sr_out),
                ..Default::default()
            },
        )
        .unwrap();
        let block = to_block(&audio);

        assert_eq!(audio.sample_rate, 48000); // Original sample rate is preserved in metadata
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
}
