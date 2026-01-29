use audioadapter_buffers::direct::InterleavedSlice;
use num::Float;
use rubato::Fft;
use rubato::Resampler as _;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResampleError {
    #[error("could not create resampler")]
    ResamplerConstructionError(#[from] rubato::ResamplerConstructionError),
    #[error("could not resample audio")]
    ResampleError(#[from] rubato::ResampleError),
}

pub fn resample<F: Float + rubato::Sample>(
    audio_interleaved: &[F],
    num_channels: usize,
    sr_in: u32,
    sr_out: u32,
) -> Result<Vec<F>, ResampleError> {
    let mut resampler = Fft::new(
        sr_in as usize,
        sr_out as usize,
        1024,
        2,
        num_channels,
        rubato::FixedSync::Both,
    )?;

    let num_input_frames = audio_interleaved.len() / num_channels;
    let buffer_in = InterleavedSlice::new(audio_interleaved, num_channels, num_input_frames)
        .expect("Should be the right size");

    let num_output_frames = resampler.process_all_needed_output_len(num_input_frames);
    let mut out_slice = vec![F::zero(); num_output_frames * num_channels];
    let mut buffer_out = InterleavedSlice::new_mut(&mut out_slice, num_channels, num_output_frames)
        .expect("should be the right size");

    // process_all_into_buffer returns (input_frames_used, output_frames_written)
    // It already trims the resampler delay internally
    let (_, actual_output_frames) =
        resampler.process_all_into_buffer(&buffer_in, &mut buffer_out, num_input_frames, None)?;

    // Truncate to actual output length
    out_slice.truncate(actual_output_frames * num_channels);
    Ok(out_slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resample_preserves_frequency() {
        use crate::reader::{AudioReadConfig, audio_read};
        use audio_blocks::{AudioBlock, AudioBlockInterleavedView};

        // Read the test file
        let audio =
            audio_read::<f32>("test_data/test_4ch.wav", AudioReadConfig::default()).unwrap();

        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(audio.num_channels, 4);

        // Resample from 48000 Hz to 22050 Hz
        let sr_out = 22050u32;
        let resampled = resample(
            &audio.samples_interleaved,
            audio.num_channels as usize,
            audio.sample_rate,
            sr_out,
        )
        .unwrap();

        let block = AudioBlockInterleavedView::from_slice(&resampled, audio.num_channels);

        // Expected frames after resampling: 48000 * (22050/48000) = 22050
        let expected_frames = 22050usize;
        assert_eq!(
            block.num_frames(),
            expected_frames,
            "Expected {} frames, got {}",
            expected_frames,
            block.num_frames()
        );

        // Verify sine wave frequencies are preserved after resampling
        // Original frequencies: [440, 554.37, 659.25, 880] Hz
        const FREQUENCIES: [f64; 4] = [440.0, 554.37, 659.25, 880.0];

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
