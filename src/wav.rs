//! A minimal WAV (RIFF) encoder.
//!
//! This covers exactly what this crate writes: interleaved 8, 16 and 32-bit
//! integer PCM plus 32-bit IEEE float. The sample count is known before the
//! first byte goes out, so every chunk size can be computed up front and the
//! encoder never has to seek back over its own output.
//!
//! The choice of `fmt ` chunk layout matters more than it looks.
//! `WAVEFORMATEXTENSIBLE` carries a `dwChannelMask` naming the physical
//! speakers that the channels belong to, and there is no way to say "one
//! channel, no particular speaker" in it. Labelling a mono file
//! `SPEAKER_FRONT_LEFT` makes players route it to the left speaker only, so
//! mono and stereo never use the extensible layout here. That matches what
//! ffmpeg and libsndfile write. Above two channels the layout is unavoidable,
//! and there the mask says the channels are unassigned rather than guessing at
//! speakers the caller never named.

use std::io::{self, Write};

use num::Float;

use crate::writer::{SampleFormat, WriteError};

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

/// Samples converted per scratch buffer fill, so that peak memory does not
/// scale with the length of the input.
const BLOCK_SAMPLES: usize = 4096;

/// Container size of one sample, in bytes.
fn bytes_per_sample(format: SampleFormat) -> u16 {
    match format {
        SampleFormat::Int8 => 1,
        SampleFormat::Int16 => 2,
        SampleFormat::Int32 | SampleFormat::Float32 => 4,
    }
}

/// The `wFormatTag` a format uses when it is not wrapped in a
/// `WAVEFORMATEXTENSIBLE`.
fn format_tag(format: SampleFormat) -> u16 {
    match format {
        SampleFormat::Int8 | SampleFormat::Int16 | SampleFormat::Int32 => WAVE_FORMAT_PCM,
        SampleFormat::Float32 => WAVE_FORMAT_IEEE_FLOAT,
    }
}

/// Whether a format needs the `fact` chunk, which the spec requires for
/// everything that is not integer PCM.
fn needs_fact_chunk(format: SampleFormat) -> bool {
    matches!(format, SampleFormat::Float32)
}

/// The `dwChannelMask` written for multichannel files.
///
/// Zero means the channels are not assigned to physical speakers, which is the
/// only honest answer available: this crate is given a channel count and told
/// nothing about the layout, and naming the wrong speakers makes players route
/// channels wrongly. Filling the mask instead, as is tempting, labels the
/// fourth channel of a quadraphonic file as the subwoofer feed.
const CHANNEL_MASK_UNASSIGNED: u32 = 0;

/// Which `fmt ` chunk layout a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    /// 16-byte `PCMWAVEFORMAT`, the most widely understood variant. Integer PCM
    /// with at most two channels.
    Pcm,
    /// 18-byte `WAVEFORMATEX`, a `PCMWAVEFORMAT` plus a zero `cbSize`. Formats
    /// other than integer PCM have to carry `cbSize`, so float files land here
    /// even in mono and stereo.
    Ex,
    /// 40-byte `WAVEFORMATEXTENSIBLE`. Needed above two channels, being the
    /// only variant that can say which speaker each channel belongs to.
    Extensible,
}

/// The byte layout of a file, resolved before anything is written.
///
/// Every field the header needs is decided here, which keeps the size
/// arithmetic in one place and lets a file that cannot be represented be
/// rejected before the output is touched.
#[derive(Debug)]
pub(crate) struct Layout {
    format: SampleFormat,
    num_channels: u16,
    sample_rate: u32,
    fmt: Fmt,
    /// Length of the `fmt ` chunk body, excluding the 8-byte chunk header.
    fmt_len: u32,
    /// Length of the `data` chunk body, excluding any trailing pad byte.
    data_len: u32,
    /// Whether a pad byte follows the `data` body to keep the file word aligned.
    data_pad: bool,
    /// The value of the RIFF size field: the length of everything after it.
    riff_len: u32,
    num_frames: u32,
    /// `nBlockAlign`, the size of one frame across all channels.
    block_align: u16,
    /// `nAvgBytesPerSec`.
    byte_rate: u32,
}

impl Layout {
    /// Resolves the layout for `num_samples` interleaved samples.
    ///
    /// `num_samples` is assumed to be a multiple of `num_channels`, which the
    /// caller validates so it can report a more specific error.
    pub(crate) fn new(
        num_samples: usize,
        num_channels: u16,
        sample_rate: u32,
        format: SampleFormat,
    ) -> Result<Self, WriteError> {
        let bytes_per_sample = bytes_per_sample(format);

        // nBlockAlign is a 16-bit field, so a wide enough frame cannot be
        // described at all. Reject it rather than writing a wrapped value.
        let block_align = u32::from(num_channels) * u32::from(bytes_per_sample);
        let block_align = u16::try_from(block_align)
            .map_err(|_| WriteError::FrameTooLarge { bytes: block_align })?;

        // Likewise nAvgBytesPerSec is 32-bit.
        let byte_rate = u64::from(sample_rate) * u64::from(block_align);
        let byte_rate = u32::try_from(byte_rate).map_err(|_| WriteError::ByteRateTooHigh {
            bytes_per_second: byte_rate,
        })?;

        let fmt = if num_channels > 2 {
            Fmt::Extensible
        } else if needs_fact_chunk(format) {
            Fmt::Ex
        } else {
            Fmt::Pcm
        };
        let fmt_len: u32 = match fmt {
            Fmt::Pcm => 16,
            Fmt::Ex => 18,
            Fmt::Extensible => 40,
        };

        let data_len = num_samples as u64 * u64::from(bytes_per_sample);
        // Chunk bodies are padded to an even length. The size field stays odd.
        let data_pad = data_len % 2 == 1;

        // "WAVE", then the fmt chunk, the optional fact chunk and the data
        // chunk, each with its 8-byte header.
        let fact_chunk: u64 = if needs_fact_chunk(format) { 12 } else { 0 };
        let riff_len =
            4 + (8 + u64::from(fmt_len)) + fact_chunk + (8 + data_len + u64::from(data_pad));

        // The RIFF and data size fields are 32-bit, which caps a wav file at
        // 4 GiB. Larger output needs RF64, which this crate does not write.
        let riff_len = u32::try_from(riff_len).map_err(|_| WriteError::FileTooLarge {
            bytes: 8 + riff_len,
        })?;

        Ok(Layout {
            format,
            num_channels,
            sample_rate,
            fmt,
            fmt_len,
            // Bounded by riff_len, which was just checked.
            data_len: data_len as u32,
            data_pad,
            riff_len,
            num_frames: (num_samples / usize::from(num_channels)) as u32,
            block_align,
            byte_rate,
        })
    }
}

/// Writes a complete wav file for `layout`.
///
/// `samples` is interleaved and must be the same slice whose length `layout`
/// was resolved from.
pub(crate) fn write<F: Float, W: Write>(
    writer: &mut W,
    layout: &Layout,
    samples: &[F],
) -> Result<(), WriteError> {
    write_header(writer, layout)?;
    write_samples(writer, samples, layout.format)?;
    if layout.data_pad {
        writer.write_all(&[0])?;
    }
    Ok(())
}

/// Writes everything up to the first sample.
fn write_header<W: Write>(writer: &mut W, layout: &Layout) -> io::Result<()> {
    // The largest possible header: 12 bytes of RIFF, a 40-byte fmt chunk, a
    // fact chunk and the data chunk header.
    let mut header = Vec::with_capacity(80);
    let bits_per_sample = bytes_per_sample(layout.format) * 8;

    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&layout.riff_len.to_le_bytes());
    header.extend_from_slice(b"WAVE");

    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&layout.fmt_len.to_le_bytes());

    // The fields shared by all three fmt layouts: WAVEFORMAT plus
    // wBitsPerSample. For the formats written here the container is always
    // exactly as wide as the samples in it.
    let tag = match layout.fmt {
        Fmt::Pcm | Fmt::Ex => format_tag(layout.format),
        Fmt::Extensible => WAVE_FORMAT_EXTENSIBLE,
    };
    header.extend_from_slice(&tag.to_le_bytes());
    header.extend_from_slice(&layout.num_channels.to_le_bytes());
    header.extend_from_slice(&layout.sample_rate.to_le_bytes());
    header.extend_from_slice(&layout.byte_rate.to_le_bytes());
    header.extend_from_slice(&layout.block_align.to_le_bytes());
    header.extend_from_slice(&bits_per_sample.to_le_bytes());

    match layout.fmt {
        Fmt::Pcm => {}
        // cbSize, with no extension following it.
        Fmt::Ex => header.extend_from_slice(&0u16.to_le_bytes()),
        Fmt::Extensible => {
            // cbSize, the number of bytes after this field.
            header.extend_from_slice(&22u16.to_le_bytes());
            // wValidBitsPerSample.
            header.extend_from_slice(&bits_per_sample.to_le_bytes());
            header.extend_from_slice(&CHANNEL_MASK_UNASSIGNED.to_le_bytes());
            header.extend_from_slice(match layout.format {
                SampleFormat::Int8 | SampleFormat::Int16 | SampleFormat::Int32 => &SUBFORMAT_PCM,
                SampleFormat::Float32 => &SUBFORMAT_IEEE_FLOAT,
            });
        }
    }

    if needs_fact_chunk(layout.format) {
        header.extend_from_slice(b"fact");
        header.extend_from_slice(&4u32.to_le_bytes());
        // dwSampleLength, counted in frames.
        header.extend_from_slice(&layout.num_frames.to_le_bytes());
    }

    header.extend_from_slice(b"data");
    header.extend_from_slice(&layout.data_len.to_le_bytes());

    writer.write_all(&header)
}

/// Scale a normalized sample to an integer range.
///
/// Rounds to the nearest integer and clamps to `[-max, max]`, so that full scale
/// input neither wraps nor drops out when the range is not exactly representable
/// in the sample type.
fn to_int<F: Float>(sample: F, max: f64) -> f64 {
    let sample = sample.to_f64().unwrap_or(0.0).clamp(-1.0, 1.0);
    (sample * max).round().clamp(-max, max)
}

/// Converts and writes the body of the data chunk.
fn write_samples<F: Float, W: Write>(
    writer: &mut W,
    samples: &[F],
    format: SampleFormat,
) -> io::Result<()> {
    let mut block = Vec::with_capacity(BLOCK_SAMPLES * usize::from(bytes_per_sample(format)));

    for chunk in samples.chunks(BLOCK_SAMPLES) {
        block.clear();
        for &sample in chunk {
            match format {
                SampleFormat::Int8 => {
                    // 8-bit wav samples are unsigned, with silence at 128.
                    let sample = to_int(sample, f64::from(i8::MAX)) as i8;
                    block.push((sample as u8).wrapping_add(128));
                }
                SampleFormat::Int16 => {
                    let sample = to_int(sample, f64::from(i16::MAX)) as i16;
                    block.extend_from_slice(&sample.to_le_bytes());
                }
                SampleFormat::Int32 => {
                    let sample = to_int(sample, f64::from(i32::MAX)) as i32;
                    block.extend_from_slice(&sample.to_le_bytes());
                }
                SampleFormat::Float32 => {
                    let sample = sample.to_f32().unwrap_or(0.0);
                    block.extend_from_slice(&sample.to_le_bytes());
                }
            }
        }
        writer.write_all(&block)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FORMATS: [SampleFormat; 4] = [
        SampleFormat::Int8,
        SampleFormat::Int16,
        SampleFormat::Int32,
        SampleFormat::Float32,
    ];

    const INT_FORMATS: [SampleFormat; 3] =
        [SampleFormat::Int8, SampleFormat::Int16, SampleFormat::Int32];

    /// A parsed wav file, so that tests can assert on structure instead of
    /// hard-coded byte offsets.
    struct Parsed {
        total_len: usize,
        riff_len: u32,
        chunks: Vec<(String, Vec<u8>)>,
    }

    impl Parsed {
        /// Walks the chunk list, which also checks that every chunk is exactly
        /// as long as it claims and that the file ends on a chunk boundary.
        fn new(bytes: &[u8]) -> Self {
            assert!(bytes.len() >= 12, "file is shorter than a RIFF header");
            assert_eq!(&bytes[0..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");

            let mut chunks = Vec::new();
            let mut pos = 12;
            while pos < bytes.len() {
                assert!(pos + 8 <= bytes.len(), "truncated chunk header at {pos}");
                let id = String::from_utf8(bytes[pos..pos + 4].to_vec()).unwrap();
                let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
                assert!(pos + 8 + len <= bytes.len(), "chunk {id} runs past the end");
                chunks.push((id, bytes[pos + 8..pos + 8 + len].to_vec()));
                // Chunk bodies are padded to an even length, while the size
                // field stays odd. Requiring the walk to land exactly on the
                // end of the file makes a missing pad byte a failure here.
                pos += 8 + len + len % 2;
            }
            assert_eq!(pos, bytes.len(), "file does not end on a chunk boundary");

            Parsed {
                total_len: bytes.len(),
                riff_len: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                chunks,
            }
        }

        fn ids(&self) -> Vec<&str> {
            self.chunks.iter().map(|(id, _)| id.as_str()).collect()
        }

        fn chunk(&self, id: &str) -> Option<&[u8]> {
            self.chunks
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, body)| body.as_slice())
        }

        fn fmt(&self) -> &[u8] {
            self.chunk("fmt ").expect("missing fmt chunk")
        }

        fn data(&self) -> &[u8] {
            self.chunk("data").expect("missing data chunk")
        }

        fn i16_samples(&self) -> Vec<i16> {
            self.data()
                .chunks(2)
                .map(|b| i16::from_le_bytes(b.try_into().unwrap()))
                .collect()
        }

        fn i32_samples(&self) -> Vec<i32> {
            self.data()
                .chunks(4)
                .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        }

        fn f32_samples(&self) -> Vec<f32> {
            self.data()
                .chunks(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        }

        fn fmt_u16(&self, offset: usize) -> u16 {
            u16::from_le_bytes(self.fmt()[offset..offset + 2].try_into().unwrap())
        }

        fn fmt_u32(&self, offset: usize) -> u32 {
            u32::from_le_bytes(self.fmt()[offset..offset + 4].try_into().unwrap())
        }

        fn tag(&self) -> u16 {
            self.fmt_u16(0)
        }
        fn num_channels(&self) -> u16 {
            self.fmt_u16(2)
        }
        fn sample_rate(&self) -> u32 {
            self.fmt_u32(4)
        }
        fn byte_rate(&self) -> u32 {
            self.fmt_u32(8)
        }
        fn block_align(&self) -> u16 {
            self.fmt_u16(12)
        }
        fn bits_per_sample(&self) -> u16 {
            self.fmt_u16(14)
        }
        fn cb_size(&self) -> u16 {
            self.fmt_u16(16)
        }
        fn valid_bits_per_sample(&self) -> u16 {
            self.fmt_u16(18)
        }
        fn channel_mask(&self) -> u32 {
            self.fmt_u32(20)
        }
        fn subformat(&self) -> &[u8] {
            &self.fmt()[24..40]
        }

        /// The `fact` chunk's frame count, if the chunk is present.
        fn fact(&self) -> Option<u32> {
            self.chunk("fact")
                .map(|body| u32::from_le_bytes(body.try_into().expect("fact chunk is 4 bytes")))
        }
    }

    fn encode<F: Float>(
        samples: &[F],
        num_channels: u16,
        sample_rate: u32,
        format: SampleFormat,
    ) -> Vec<u8> {
        let layout = Layout::new(samples.len(), num_channels, sample_rate, format)
            .expect("layout should be representable");
        let mut out = Vec::new();
        write(&mut out, &layout, samples).expect("writing to a Vec cannot fail");
        out
    }

    /// Encodes and parses, asserting the invariants that must hold for every
    /// file this encoder produces.
    fn parse<F: Float>(
        samples: &[F],
        num_channels: u16,
        sample_rate: u32,
        format: SampleFormat,
    ) -> Parsed {
        let parsed = Parsed::new(&encode(samples, num_channels, sample_rate, format));

        assert_eq!(
            parsed.riff_len as usize,
            parsed.total_len - 8,
            "the RIFF size field must cover everything after it"
        );
        assert_eq!(parsed.total_len % 2, 0, "a wav file has an even length");
        assert_eq!(
            parsed.data().len(),
            samples.len() * usize::from(bytes_per_sample(format))
        );

        parsed
    }

    /// The bug this encoder exists for. A mono float file must not be tagged
    /// `WAVEFORMATEXTENSIBLE`, because its channel mask can only name a
    /// physical speaker, and naming one sends the audio to that speaker alone.
    #[test]
    fn mono_float_is_not_extensible() {
        let parsed = parse(&[0.0f32; 4], 1, 48_000, SampleFormat::Float32);

        assert_eq!(parsed.tag(), WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!(parsed.fmt().len(), 18, "WAVEFORMATEX");
        assert_eq!(parsed.cb_size(), 0);
    }

    /// The same bug, which also reaches every mono format wider than 16 bits.
    #[test]
    fn mono_int_is_not_extensible() {
        for format in INT_FORMATS {
            let parsed = parse(&[0.0f32; 4], 1, 48_000, format);

            assert_eq!(parsed.tag(), WAVE_FORMAT_PCM, "{format:?}");
            assert_eq!(parsed.fmt().len(), 16, "PCMWAVEFORMAT for {format:?}");
        }
    }

    /// The invariant that keeps the bug from coming back, stated directly: no
    /// file that a channel mask cannot describe may carry one.
    #[test]
    fn no_mono_or_stereo_file_carries_a_channel_mask() {
        for format in ALL_FORMATS {
            for num_channels in 1..=2 {
                let parsed = parse(&[0.0f32; 4], num_channels, 48_000, format);

                assert_ne!(
                    parsed.tag(),
                    WAVE_FORMAT_EXTENSIBLE,
                    "{num_channels} channel {format:?}"
                );
                assert_eq!(parsed.tag(), format_tag(format));
                // Nothing beyond a zero cbSize follows the shared fields.
                assert!(parsed.fmt().len() <= 18);
            }
        }
    }

    /// Above two channels the speaker assignment has to be spelled out, so the
    /// extensible layout is both allowed and required.
    #[test]
    fn multichannel_uses_extensible() {
        for format in ALL_FORMATS {
            for num_channels in 3..=8u16 {
                let samples = vec![0.0f32; usize::from(num_channels) * 2];
                let parsed = parse(&samples, num_channels, 48_000, format);
                let label = format!("{num_channels} channel {format:?}");

                assert_eq!(parsed.tag(), WAVE_FORMAT_EXTENSIBLE, "{label}");
                assert_eq!(parsed.fmt().len(), 40, "{label}");
                assert_eq!(parsed.cb_size(), 22, "{label}");
                assert_eq!(
                    parsed.valid_bits_per_sample(),
                    parsed.bits_per_sample(),
                    "{label}"
                );
                assert_eq!(parsed.channel_mask(), 0, "{label}");
                assert_eq!(
                    parsed.subformat(),
                    match format {
                        SampleFormat::Float32 => &SUBFORMAT_IEEE_FLOAT,
                        _ => &SUBFORMAT_PCM,
                    },
                    "{label}"
                );
            }
        }
    }

    /// A filled mask would name speakers the caller never chose. For four
    /// channels it claims front centre and a subwoofer feed, so the mask stays
    /// empty however many channels there are.
    #[test]
    fn multichannel_channels_are_left_unassigned() {
        for num_channels in [3u16, 4, 6, 8, 18, 19, 64] {
            let samples = vec![0.0f32; usize::from(num_channels)];
            let parsed = parse(&samples, num_channels, 48_000, SampleFormat::Int16);

            assert_eq!(parsed.channel_mask(), 0, "{num_channels} channels");
            assert_eq!(parsed.num_channels(), num_channels);
        }
    }

    #[test]
    fn fmt_chunk_describes_the_stream() {
        // A three channel 8-bit file has an odd nBlockAlign, which is the case
        // most likely to be quietly rounded somewhere.
        let parsed = parse(&[0.0f32; 6], 3, 44_100, SampleFormat::Int8);
        assert_eq!(parsed.num_channels(), 3);
        assert_eq!(parsed.sample_rate(), 44_100);
        assert_eq!(parsed.bits_per_sample(), 8);
        assert_eq!(parsed.block_align(), 3);
        assert_eq!(parsed.byte_rate(), 44_100 * 3);

        let parsed = parse(&[0.0f32; 4], 2, 96_000, SampleFormat::Float32);
        assert_eq!(parsed.num_channels(), 2);
        assert_eq!(parsed.sample_rate(), 96_000);
        assert_eq!(parsed.bits_per_sample(), 32);
        assert_eq!(parsed.block_align(), 8);
        assert_eq!(parsed.byte_rate(), 96_000 * 8);

        let parsed = parse(&[0.0f32; 4], 1, 8_000, SampleFormat::Int16);
        assert_eq!(parsed.block_align(), 2);
        assert_eq!(parsed.byte_rate(), 16_000);
    }

    /// `parse` checks the RIFF size field on every call, so sweeping the shapes
    /// that change the header length is what gives the check its coverage.
    #[test]
    fn riff_size_holds_across_shapes() {
        for format in ALL_FORMATS {
            for num_channels in [1u16, 2, 3, 6, 17, 18, 19] {
                for num_frames in [0usize, 1, 3, 1000] {
                    let samples = vec![0.25f32; usize::from(num_channels) * num_frames];
                    let parsed = parse(&samples, num_channels, 48_000, format);
                    assert_eq!(
                        parsed.data().len(),
                        samples.len() * usize::from(bytes_per_sample(format))
                    );
                }
            }
        }
    }

    /// An odd data chunk needs a trailing pad byte, while its size field must
    /// keep reporting the real, odd length.
    #[test]
    fn odd_data_chunk_is_padded() {
        let bytes = encode(&[1.0f32, 0.0, -1.0], 1, 48_000, SampleFormat::Int8);
        let parsed = Parsed::new(&bytes);

        assert_eq!(parsed.data().len(), 3, "the size field stays odd");
        assert_eq!(bytes.len() % 2, 0, "the file is padded to an even length");
        assert_eq!(bytes[bytes.len() - 1], 0, "the pad byte is zero");
        // The pad byte counts towards the RIFF size but not the data size.
        assert_eq!(parsed.riff_len as usize, bytes.len() - 8);
    }

    #[test]
    fn even_data_chunk_is_not_padded() {
        let bytes = encode(&[1.0f32, 0.0, -1.0, 0.0], 1, 48_000, SampleFormat::Int8);
        let parsed = Parsed::new(&bytes);

        assert_eq!(parsed.data().len(), 4);
        assert_eq!(parsed.riff_len as usize, bytes.len() - 8);
        // A header, four samples and nothing else.
        assert_eq!(bytes.len(), 44 + 4);
    }

    /// Only the widths that can produce an odd data chunk are 8-bit, so sweep
    /// the frame counts that expose it.
    #[test]
    fn padding_follows_the_data_length() {
        for num_frames in 0..8usize {
            let samples = vec![0.0f32; num_frames];
            let parsed = parse(&samples, 1, 48_000, SampleFormat::Int8);
            assert_eq!(parsed.data().len(), num_frames);
        }
    }

    /// Non-PCM formats carry a `fact` chunk with the frame count, and integer
    /// PCM does not.
    #[test]
    fn fact_chunk_only_accompanies_float() {
        let parsed = parse(&[0.0f32; 8], 4, 48_000, SampleFormat::Float32);
        assert_eq!(parsed.fact(), Some(2), "8 samples over 4 channels");

        let parsed = parse(&[0.0f32; 5], 1, 48_000, SampleFormat::Float32);
        assert_eq!(parsed.fact(), Some(5));

        for format in INT_FORMATS {
            let parsed = parse(&[0.0f32; 8], 4, 48_000, format);
            assert_eq!(parsed.fact(), None, "{format:?}");
        }
    }

    #[test]
    fn chunks_appear_in_the_expected_order() {
        let parsed = parse(&[0.0f32; 4], 1, 48_000, SampleFormat::Float32);
        assert_eq!(parsed.ids(), ["fmt ", "fact", "data"]);

        let parsed = parse(&[0.0f32; 4], 1, 48_000, SampleFormat::Int16);
        assert_eq!(parsed.ids(), ["fmt ", "data"]);
    }

    /// An empty file is still a well formed one.
    #[test]
    fn empty_input_produces_a_valid_header() {
        for format in ALL_FORMATS {
            for num_channels in [1u16, 2, 4] {
                let parsed = parse::<f32>(&[], num_channels, 48_000, format);

                assert!(parsed.data().is_empty(), "{format:?}");
                assert_eq!(parsed.num_channels(), num_channels);
                if needs_fact_chunk(format) {
                    assert_eq!(parsed.fact(), Some(0));
                }
            }
        }
    }

    /// 8-bit wav is the one width that is unsigned, with silence at 128 rather
    /// than 0. Getting this wrong inverts and offsets the audio.
    #[test]
    fn int8_samples_are_unsigned() {
        let parsed = parse(&[1.0f32, 0.0, -1.0], 1, 48_000, SampleFormat::Int8);
        assert_eq!(parsed.data(), [0xff, 0x80, 0x01]);
    }

    /// Full scale input must reach full scale output without wrapping to the
    /// opposite sign, which is what makes the range deliberately symmetric.
    #[test]
    fn full_scale_does_not_wrap() {
        let full_scale = [1.0f32, -1.0];

        let parsed = parse(&full_scale, 1, 48_000, SampleFormat::Int16);
        assert_eq!(parsed.i16_samples(), [i16::MAX, -i16::MAX]);

        let parsed = parse(&full_scale, 1, 48_000, SampleFormat::Int32);
        assert_eq!(parsed.i32_samples(), [i32::MAX, -i32::MAX]);

        let parsed = parse(&full_scale, 1, 48_000, SampleFormat::Int8);
        assert_eq!(parsed.data(), [0xff, 0x01]);
    }

    /// Input beyond full scale clamps rather than wrapping around.
    #[test]
    fn out_of_range_input_is_clamped() {
        let loud = [2.0f32, -2.0, 1e30, -1e30, f32::INFINITY, f32::NEG_INFINITY];

        let parsed = parse(&loud, 1, 48_000, SampleFormat::Int16);
        let max = i16::MAX;
        assert_eq!(parsed.i16_samples(), [max, -max, max, -max, max, -max]);

        let parsed = parse(&loud, 1, 48_000, SampleFormat::Int8);
        assert_eq!(parsed.data(), [0xff, 0x01, 0xff, 0x01, 0xff, 0x01]);
    }

    /// A NaN cannot be scaled into an integer, so it lands on silence instead
    /// of an arbitrary value.
    #[test]
    fn nan_becomes_silence_in_integer_formats() {
        let parsed = parse(&[f32::NAN, 0.5], 1, 48_000, SampleFormat::Int16);
        assert_eq!(parsed.i16_samples(), [0, 16384]);

        let parsed = parse(&[f32::NAN], 1, 48_000, SampleFormat::Int8);
        assert_eq!(parsed.data(), [0x80]);

        let parsed = parse(&[f32::NAN], 1, 48_000, SampleFormat::Int32);
        assert_eq!(parsed.i32_samples(), [0]);
    }

    /// Float output is a passthrough, so it neither clamps nor rounds.
    #[test]
    fn float_samples_are_bit_exact() {
        let samples = [0.1f32, -0.0, 1.5, -2.5, f32::MIN_POSITIVE];
        let parsed = parse(&samples, 1, 48_000, SampleFormat::Float32);

        let written = parsed.f32_samples();
        assert_eq!(written, samples);
        // Including the sign of a negative zero.
        assert!(written[1].is_sign_negative());
    }

    /// The integer formats fold NaN and out of range input to something
    /// representable, but float output has nothing to fold them to, so they are
    /// written verbatim rather than quietly turning into silence.
    #[test]
    fn non_finite_floats_pass_through() {
        let samples = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        let written = parse(&samples, 1, 48_000, SampleFormat::Float32).f32_samples();

        assert!(written[0].is_nan());
        assert_eq!(written[1], f32::INFINITY);
        assert_eq!(written[2], f32::NEG_INFINITY);

        // The same holds for f64 input, where the conversion to f32 is what
        // could lose them.
        let wide = [f64::NAN, 1e300, -1e300];
        let written = parse(&wide, 1, 48_000, SampleFormat::Float32).f32_samples();

        assert!(written[0].is_nan());
        assert_eq!(written[1], f32::INFINITY, "beyond f32 range");
        assert_eq!(written[2], f32::NEG_INFINITY);
    }

    /// Scaling rounds to the nearest integer rather than truncating towards
    /// zero, which would bias the signal.
    #[test]
    fn scaling_rounds_to_nearest() {
        // 0.5 * 32767 is 16383.5, which rounds away from zero.
        let parsed = parse(&[0.5f32, -0.5], 1, 48_000, SampleFormat::Int16);
        assert_eq!(parsed.i16_samples(), [16384, -16384]);

        // A value far below one LSB collapses to silence rather than to one.
        let parsed = parse(&[1e-9f32], 1, 48_000, SampleFormat::Int16);
        assert_eq!(parsed.i16_samples(), [0]);
    }

    /// The encoder is generic over the float type, so f64 input has to travel
    /// the same path and keep its extra precision on the way to the scaler.
    #[test]
    fn f64_input_is_encoded_identically() {
        for format in ALL_FORMATS {
            let wide = parse(&[1.0f64, 0.5, 0.0, -0.5, -1.0], 1, 48_000, format);
            let narrow = parse(&[1.0f32, 0.5, 0.0, -0.5, -1.0], 1, 48_000, format);
            assert_eq!(wide.data(), narrow.data(), "{format:?}");
        }

        // f64 precision beyond f32 survives as far as the integer scaler.
        let parsed = parse(&[1.0f64 / 3.0], 1, 48_000, SampleFormat::Int32);
        assert_eq!(
            parsed.i32_samples(),
            [(f64::from(i32::MAX) / 3.0).round() as i32]
        );
    }

    /// Interleaving is preserved verbatim, since a swap here would be silent.
    #[test]
    fn samples_are_written_in_order() {
        let samples: Vec<f32> = (0..12i16).map(|i| f32::from(i) / 100.0).collect();
        let parsed = parse(&samples, 3, 48_000, SampleFormat::Float32);

        assert_eq!(parsed.f32_samples(), samples);
    }

    /// Inputs longer than the internal scratch buffer are written in blocks, so
    /// check that no sample is dropped or repeated at a block boundary.
    #[test]
    fn input_spanning_several_blocks_is_written_whole() {
        // Deliberately not a multiple of BLOCK_SAMPLES.
        let samples: Vec<f32> = (0..BLOCK_SAMPLES * 2 + 7)
            .map(|i| (i % 1000) as f32 / 1000.0)
            .collect();
        let parsed = parse(&samples, 1, 48_000, SampleFormat::Float32);

        assert_eq!(parsed.f32_samples(), samples);
    }

    /// A wav file cannot describe more than 4 GiB, and the size fields wrap
    /// silently if that is not caught.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layout_rejects_files_over_four_gib() {
        let too_many = u32::MAX as usize;
        match Layout::new(too_many, 1, 48_000, SampleFormat::Int32) {
            Err(WriteError::FileTooLarge { bytes }) => {
                assert!(bytes > u64::from(u32::MAX));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Everything the RIFF size field covers apart from the data body, for a
    /// mono 8-bit file: "WAVE", the fmt chunk with its header, and the data
    /// chunk header. This is the smallest header the encoder can write, so it
    /// leaves the most room for samples.
    #[cfg(target_pointer_width = "64")]
    const MONO_PCM_OVERHEAD: usize = 4 + (8 + 16) + 8;

    /// The largest representable file must still be accepted, so that the limit
    /// is a limit and not an off-by-one.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layout_accepts_the_largest_representable_file() {
        // Rounded down to an even length, because an odd data chunk spends its
        // last byte on padding instead of a sample.
        let largest = (u32::MAX as usize - MONO_PCM_OVERHEAD) & !1;

        let layout = Layout::new(largest, 1, 48_000, SampleFormat::Int8)
            .expect("the largest file should be representable");
        assert!(!layout.data_pad, "an even length needs no pad byte");
        assert_eq!(layout.data_len as usize, largest);
        assert_eq!(layout.riff_len as usize, MONO_PCM_OVERHEAD + largest);

        assert!(matches!(
            Layout::new(largest + 2, 1, 48_000, SampleFormat::Int8),
            Err(WriteError::FileTooLarge { .. })
        ));
    }

    /// The pad byte is part of the file, so it has to be counted before the
    /// limit is checked. Without it this input lands exactly on the limit and
    /// would be accepted, then write one byte too many.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layout_counts_the_pad_byte_towards_the_limit() {
        let odd = u32::MAX as usize - MONO_PCM_OVERHEAD;
        assert_eq!(odd % 2, 1, "this case needs an odd data length");

        match Layout::new(odd, 1, 48_000, SampleFormat::Int8) {
            // The 8 bytes of "RIFF" and the size field, the limit itself, and
            // the pad byte that overshot it.
            Err(WriteError::FileTooLarge { bytes }) => {
                assert_eq!(bytes, 8 + u64::from(u32::MAX) + 1);
            }
            other => panic!("{other:?}"),
        }
    }

    /// `nBlockAlign` is 16-bit, so a wide enough frame cannot be described.
    #[test]
    fn layout_rejects_oversized_frames() {
        match Layout::new(0, u16::MAX, 48_000, SampleFormat::Int32) {
            Err(WriteError::FrameTooLarge { bytes }) => {
                assert_eq!(bytes, u32::from(u16::MAX) * 4);
            }
            other => panic!("{other:?}"),
        }

        // The widest frame that still fits is accepted.
        let layout = Layout::new(0, u16::MAX, 8_000, SampleFormat::Int8).unwrap();
        assert_eq!(layout.block_align, u16::MAX);

        assert!(matches!(
            Layout::new(0, 16_384, 8_000, SampleFormat::Int32),
            Err(WriteError::FrameTooLarge { .. })
        ));
        assert!(Layout::new(0, 16_383, 8_000, SampleFormat::Int32).is_ok());
    }

    /// `nAvgBytesPerSec` is 32-bit, which a wide frame at a high rate can
    /// overflow even when the frame itself fits.
    #[test]
    fn layout_rejects_excessive_byte_rates() {
        // 65535 channels of 8-bit audio is the widest describable frame, so it
        // needs the lowest sample rate to overflow the byte rate.
        match Layout::new(0, u16::MAX, 65_538, SampleFormat::Int8) {
            Err(WriteError::ByteRateTooHigh { bytes_per_second }) => {
                assert!(bytes_per_second > u64::from(u32::MAX));
            }
            other => panic!("{other:?}"),
        }

        // One rate lower the same shape lands exactly on the limit.
        let layout = Layout::new(0, u16::MAX, 65_537, SampleFormat::Int8).unwrap();
        assert_eq!(layout.byte_rate, u32::MAX);
    }

    #[test]
    fn layout_counts_frames_not_samples() {
        let layout = Layout::new(12, 4, 48_000, SampleFormat::Int16).unwrap();
        assert_eq!(layout.num_frames, 3);
        assert_eq!(layout.data_len, 24);

        let layout = Layout::new(0, 4, 48_000, SampleFormat::Int16).unwrap();
        assert_eq!(layout.num_frames, 0);
    }
}
