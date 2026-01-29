#![doc = include_str!("../README.md")]

#[cfg(feature = "audio-blocks")]
pub use audio_blocks::*;

#[cfg(feature = "audio-blocks")]
pub use reader::audio_read_block;
pub use reader::{Audio, AudioReadConfig, AudioReadError, Position, audio_read};
#[cfg(feature = "audio-blocks")]
pub use writer::audio_write_block;
pub use writer::{AudioWriteConfig, AudioWriteError, audio_write};

pub mod reader;
pub mod resample;
pub mod writer;
