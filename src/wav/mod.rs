//! Everything specific to the WAV (RIFF) format: the encoder this crate
//! writes with, and a decoder for the fast path in [`crate::reader`].
//!
//! Both sides need the same handful of format tags and subformat GUIDs, so
//! they live here rather than in either submodule.

mod decode;
mod encode;

pub(crate) use decode::{open_wav, read_frames};
pub(crate) use encode::{Layout, write};

/// Integer PCM samples.
const WAVE_FORMAT_PCM: u16 = 0x0001;
/// IEEE float samples.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// The `fmt ` chunk holds a `WAVEFORMATEXTENSIBLE`, and the real format is
/// named by its `SubFormat` GUID instead.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

/// `KSDATAFORMAT_SUBTYPE_PCM`, the `SubFormat` GUID for integer PCM.
const SUBFORMAT_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`, the `SubFormat` GUID for float samples.
const SUBFORMAT_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
