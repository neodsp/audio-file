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
//! Wav files holding integer PCM or IEEE float samples, which is everything this crate itself
//! writes, are read by a built-in decoder that needs no feature and no dependency. Every other
//! format, and the wav encodings the built-in decoder does not cover, is read through Symphonia,
//! which each of the codec features below pulls in.
//!
//! With default features off you therefore get a wav-only crate with no Symphonia in the
//! dependency tree at all. A file it cannot read then fails with [`ReadError::UnsupportedFormat`]
//! rather than being handed on. Default features enable all codecs (including royalty-encumbered
//! formats) via `all-codecs` and Symphonia's SIMD optimizations via `simd`.
//!
//! Every feature flag in the three tables below except `wav-compressed` names a Symphonia feature
//! directly, and each maps to exactly one Symphonia crate, which falls into one of three kinds. A
//! container crate (`symphonia-format-*`) only demuxes; it locates the packets of whatever codec
//! is inside and cannot decode any of them itself. A codec crate (`symphonia-codec-*`) is the
//! reverse: it only decodes, and needs a container to hand it packets. A bundle crate
//! (`symphonia-bundle-*`) is both in one, because that format never appears inside another
//! container.
//!
//! Enabling a container flag alone therefore builds a demuxer with nothing to decode with, the
//! same trap `wav-compressed` exists to avoid for wav specifically. A file needs one flag from the
//! self-contained table, or one flag from each of the container and codec tables.
//!
//! Self-contained, one flag reads the format:
//!
//! | Format | Feature Flag |
//! |--------|--------------|
//! | WAV, integer PCM and IEEE float | none, built in |
//! | WAV, A-law, mu-law and ADPCM | `wav-compressed` |
//! | FLAC | `flac` |
//! | MP1 | `mp1` |
//! | MP2 | `mp2` |
//! | MP3 | `mp3` |
//!
//! Containers, demux only - pair with a codec flag from the next table:
//!
//! | Container | Feature Flag | Commonly holds |
//! |-----------|--------------|-----------------|
//! | AIFF | `aiff` | PCM |
//! | CAF | `caf` | PCM, ALAC |
//! | ISO MP4 | `isomp4` | AAC, ALAC |
//! | Matroska (MKV) | `mkv` | PCM, FLAC, Vorbis, AAC, ALAC |
//! | Ogg | `ogg` | Vorbis, FLAC |
//!
//! Codecs, decode only - pair with a container flag from the table above:
//!
//! | Codec | Feature Flag |
//! |-------|--------------|
//! | AAC | `aac` |
//! | ADPCM | `adpcm` |
//! | ALAC | `alac` |
//! | PCM | `pcm` |
//! | Vorbis | `vorbis` |
//!
//! Feature flags:
//!
//! - `all-codecs` enables all Symphonia codecs (enabled by default). It does not by itself enable
//!   any container, so pair it with the containers you need.
//! - `wav-compressed` adds the wav encodings the built-in decoder does not cover: A-law, mu-law and
//!   ADPCM. There is deliberately no plain `wav` feature: it would be a container flag with no
//!   codec paired in by default, the same trap as enabling `aiff` or `caf` alone. `wav-compressed`
//!   bundles the demuxer with `pcm` and `adpcm`, which is what reading those files actually takes.
//! - `simd` enables Symphonia's SIMD optimizations (enabled by default). It is a modifier, so it
//!   does nothing on its own in a build without Symphonia.
//! - `symphonia` pulls Symphonia in. Every codec and container flag enables it, so it is rarely
//!   named directly.
//! - `audio-blocks` enables `read_block` and `write_block`.
//!
//! ## Known Limitations
//!
//! WAV files with integer PCM or IEEE float samples, which is everything this crate itself
//! writes, are read by the built-in decoder, which has no channel ceiling. Every other format is
//! decoded through Symphonia, which maps channels to named speaker positions instead of treating
//! them as a plain count. Several containers therefore have a read-back channel ceiling well
//! below what the format itself allows:
//!
//! - **WAV** files the built-in decoder cannot handle - ADPCM, A-law/mu-law and other compressed
//!   encodings - fall back to Symphonia, which is rejected above 18 channels for an extensible
//!   `fmt` chunk, or 26 for a plain one. Without `wav-compressed` they are not read at all.
//! - **CAF** files with high channel counts are unreliable: a 24-channel file is silently
//!   decoded as 18 channels with misaligned samples, and 32 channels is rejected.
//! - **Matroska** has no such ceiling, since channels are treated as discrete. 24 and 32
//!   channel files read back correctly.
//! - **FLAC** is limited to 8 channels by the format itself, so it is no alternative for high
//!   channel counts.
//!
//! For high channel counts in a compressed format, prefer Matroska.
//!
//! ## Read and Write Options
//!
//! ### Reading
//!
//! When reading a file you can specify the following things:
//!
//! - Start and stop in frames or time
//! - Start channel and number of channels
//! - Optional resampling
//!
//! Only selected frames are stored. The reader may decode and discard earlier packets for accurate
//! seeking and codec warm-up.
//!
//! The start position is inclusive and the stop position is exclusive, so reading from frame 300
//! to frame 400 yields 100 frames. Frame 0 is the first playable frame: encoder delay and padding,
//! as used by formats like MP3, are not part of the timeline.
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
pub use resample::ResampleError;
#[cfg(feature = "audio-blocks")]
pub use writer::write_block;
pub use writer::{SampleFormat, WriteConfig, WriteError, write};

pub mod reader;
mod resample;
mod wav;
pub mod writer;

/// A unique temporary path for a test file, so that concurrent test runs and
/// leftovers from a panicked run cannot interfere with each other.
#[cfg(test)]
pub(crate) fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("audio-file-{}-{name}", std::process::id()))
}
