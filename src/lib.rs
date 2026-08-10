//! # audio-file
//!
//! A simple library to read and write audio files on your disk.
//!
//! The library can read many formats and can write only to wav files.
//!
//! ## Quick Start
//!
//! ### Read Audio
//!
//! You can read most common audio formats. The default feature set enables all available codecs.
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let audio = audio_file::read::<f32>("test_data/test_1ch.wav", audio_file::ReadConfig::default())?;
//! let sample_rate = audio.sample_rate;
//! let num_channels = audio.num_channels;
//! let samples = &audio.samples_interleaved;
//! # Ok(())
//! # }
//! ```
//!
//! With `audio-blocks`, you can read straight into an `AudioBlock`, which adds simple channel-based read helpers:
//!
//! ```rust
//! # #[cfg(feature = "audio-blocks")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let (block, sample_rate) = audio_file::read_block::<f32>("test_data/test_1ch.wav", audio_file::ReadConfig::default())?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "audio-blocks"))]
//! # fn main() {}
//! ```
//!
//! ### Write Audio
//!
//! You can only write wav files. The `audio_file::write` function expects interleaved samples.
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let samples = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]; // interleaved
//! let num_channels = 2;
//! let sample_rate = 48000;
//! audio_file::write(
//!     "output.wav",
//!     &samples,
//!     num_channels,
//!     sample_rate,
//!     audio_file::WriteConfig::default(),
//! )?;
//! # std::fs::remove_file("output.wav")?;
//! # Ok(())
//! # }
//! ```
//!
//! With the `audio-blocks` feature you can write any audio layout, e.g.:
//!
//! ```rust
//! # #[cfg(feature = "audio-blocks")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use audio_file::*;
//! let sample_rate = 48000;
//!
//! let block = Interleaved::from_slice(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0], 2);
//! audio_file::write_block("output_layout.wav", block, sample_rate, audio_file::WriteConfig::default())?;
//!
//! let block = Sequential::from_slice(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0], 2);
//! audio_file::write_block("output_layout.wav", block, sample_rate, audio_file::WriteConfig::default())?;
//! # std::fs::remove_file("output_layout.wav")?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "audio-blocks"))]
//! # fn main() {}
//! ```
//!
//! ## Supported Input Formats
//!
//! The default features read every format below. Turn them off to pick only what you need:
//!
//! | Format | Feature flags |
//! |--------|---------------|
//! | WAV, integer PCM and IEEE float | none, built in |
//! | WAV, A-law, mu-law and ADPCM | `wav-compressed` |
//! | FLAC | `flac` |
//! | MP1, MP2, MP3 | `mp1`, `mp2`, `mp3` |
//! | AAC (raw ADTS) | `aac` |
//! | MP4, M4A | `isomp4` plus `aac` or `alac` |
//! | Ogg | `ogg` plus `vorbis` or `flac` |
//! | Matroska (MKA, MKV) | `mkv` plus the codec inside: `pcm`, `flac`, `vorbis`, `aac`, `alac` |
//! | AIFF | `aiff` plus `pcm` |
//! | CAF | `caf` plus `pcm` or `alac` |
//!
//! A container and the codec inside it are separate flags, so a format that can hold several
//! codecs needs one of each. Enabling only the container reads no file at all.
//!
//! Other flags:
//!
//! - `all-codecs` (default) enables every format in the table above.
//! - `simd` (default) enables Symphonia's SIMD optimizations.
//! - `resample` (default) enables resampling while reading, via `rubato`.
//! - `audio-blocks` enables `read_block` and `write_block`.
//!
//! With default features off, wav with integer PCM or IEEE float samples, which is everything this
//! crate writes, is still read by a built-in decoder, and neither Symphonia nor `rubato` is in the
//! dependency tree at all: eight crates instead of forty-six. Any other file then fails with
//! [`ReadError::UnsupportedFormat`], and [`ReadConfig`] loses its `sample_rate` field along with
//! the resampler, so asking for a rate nothing would resample to does not compile.
//!
//! ## Known Limitations
//!
//! Symphonia maps channels to named speaker positions rather than treating them as a plain count,
//! so formats read through it have channel ceilings below what the format itself allows. Wav files
//! read by the built-in decoder have no ceiling, and neither does Matroska, so prefer Matroska for
//! high channel counts in a compressed format.
//!
//! - **WAV** files the built-in decoder cannot handle, so ADPCM and A-law/mu-law, are rejected
//!   above 18 channels for an extensible `fmt ` chunk, or 26 for a plain one.
//! - **CAF** is unreliable above 18 channels: a 24-channel file decodes as 18 channels with
//!   misaligned samples and no error at all, and 32 channels is rejected.
//! - **FLAC** is capped at 8 channels by the format itself.
//!
//! ## Read and Write Options
//!
//! ### Reading
//!
//! When reading a file you can specify the following things:
//!
//! - Start and stop in frames or time
//! - Start channel and number of channels
//! - Optional resampling, with the `resample` feature
//!
//! Only selected frames are stored. The reader may decode and discard earlier packets for accurate
//! seeking and codec warm-up.
//!
//! The start position is inclusive and the stop position is exclusive, so reading from frame 300
//! to frame 400 yields 100 frames. Frame 0 is the first playable frame: encoder delay and padding,
//! as used by formats like MP3, are not part of the timeline.
//!
//! ### Damaged Files
//!
//! A file is either read in full or not at all. A packet the decoder rejects, because the file is
//! damaged, was truncated mid-transfer, or holds an encoding this build has no codec for, ends the
//! read with [`ReadError::Decode`]. Nothing is skipped over or filled in.
//!

//! ### Writing
//!
//! For writing audio you can select from the following sample formats:
//!
//! | Format | Description |
//! |--------|-------------|
//! | `Int8` | 8-bit integer |
//! | `Int16` | 16-bit integer (default, for the broadest compatibility) |
//! | `Int32` | 32-bit integer |
//! | `Float32` | 32-bit float |
//!
//! ### Some example configs:
//!
//! - resample to 22.05 kHz while reading
//!
//! ```rust
//! # #[cfg(feature = "resample")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let audio = audio_file::read::<f32>(
//!     "test_data/test_1ch.wav",
//!     audio_file::ReadConfig {
//!         sample_rate: Some(22_050),
//!         ..Default::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "resample"))]
//! # fn main() {}
//! ```
//!
//! - read the first 0.5 seconds
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use std::time::Duration;
//! let audio = audio_file::read::<f32>(
//!     "test_data/test_1ch.wav",
//!     audio_file::ReadConfig {
//!         stop: audio_file::Position::Time(Duration::from_secs_f32(0.5)),
//!         ..Default::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! - read from frame 300 to 400
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use audio_file::*;
//! let audio = audio_file::read::<f32>(
//!     "test_data/test_1ch.wav",
//!     audio_file::ReadConfig {
//!         start: audio_file::Position::Frame(300),
//!         stop: audio_file::Position::Frame(400),
//!         ..Default::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! - read only the first two channels
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use audio_file::*;
//! let audio = audio_file::read::<f32>(
//!     "test_data/test_4ch.wav",
//!     audio_file::ReadConfig {
//!         num_channels: Some(2),
//!         ..Default::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! - skip the first channel, reading channel 2 and 3
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let audio = audio_file::read::<f32>(
//!     "test_data/test_4ch.wav",
//!     audio_file::ReadConfig {
//!         start_channel: Some(1),
//!         num_channels: Some(2),
//!         ..Default::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! - write audio samples in `Float32`
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let samples_interleaved: Vec<f32> = vec![];
//! # let num_channels = 2u16;
//! # let sample_rate = 48000u32;
//! audio_file::write(
//!     "output_float32.wav",
//!     &samples_interleaved,
//!     num_channels,
//!     sample_rate,
//!     audio_file::WriteConfig {
//!         sample_format: audio_file::SampleFormat::Float32,
//!     },
//! )?;
//! # std::fs::remove_file("output_float32.wav")?;
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "audio-blocks")]
pub use audio_blocks::*;

#[cfg(feature = "audio-blocks")]
pub use reader::read_block;
pub use reader::{Audio, Position, ReadConfig, ReadError, read};
#[cfg(feature = "resample")]
pub use resample::ResampleError;
#[cfg(feature = "audio-blocks")]
pub use writer::write_block;
pub use writer::{SampleFormat, WriteConfig, WriteError, write};

pub mod reader;
#[cfg(feature = "resample")]
mod resample;
mod wav;
pub mod writer;

/// A unique temporary path for a test file, so that concurrent test runs and
/// leftovers from a panicked run cannot interfere with each other.
#[cfg(test)]
pub(crate) fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("audio-file-{}-{name}", std::process::id()))
}
