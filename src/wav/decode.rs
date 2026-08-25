//! A minimal WAV (RIFF) decoder for the fast path in [`crate::reader`].
//!
//! PCM audio in a WAV file is a flat byte array: frame `n` of channel `c`
//! starts at `data_start + n * block_align + c * container_bytes`. That is
//! the whole format. There are no packet timestamps, no decoder warm-up and
//! no seek landing to verify, so a frame range and a channel range are just
//! that formula, unlike the general path in [`crate::reader`] which has to
//! recover both from a codec's bitstream.
//!
//! This only covers what it can decode without ambiguity: 8/16/24/32-bit
//! integer PCM (including 24-bit samples stored in a 4-byte container, which
//! is nonstandard but common) and 32/64-bit IEEE float. Anything else -
//! ADPCM, A-law/mu-law, an unrecognised format tag, or a file that is not
//! RIFF/WAVE at all - is reported as [`None`] so the caller can fall back to
//! the general decoder instead of failing the read outright.
//!
//! Two fields of an extensible `fmt ` chunk are deliberately not trusted the
//! way a reader might expect:
//!
//! - `dwChannelMask` is ignored entirely. This crate reports a channel count
//!   and never a speaker layout, so the mask has nothing to contribute, and
//!   it is precisely the attempt to map it onto named speaker positions that
//!   puts a channel ceiling on the general decoder.
//! - `wValidBitsPerSample` does not change how bytes are read. It states how
//!   many bits carry signal, and those bits are left-justified within the
//!   container, so a 32-bit container holding 24 valid bits is read as a
//!   32-bit sample and normalizes correctly on its own. It is consulted only
//!   when `wBitsPerSample` is zero and there is nothing else to go on.

use std::io::{self, Read, Seek, SeekFrom};

use num_traits::Float;

use super::{
    SUBFORMAT_IEEE_FLOAT, SUBFORMAT_PCM, WAVE_FORMAT_EXTENSIBLE, WAVE_FORMAT_IEEE_FLOAT,
    WAVE_FORMAT_PCM,
};
use crate::reader::MAX_PREALLOC_SAMPLES;

/// Whether the container holds integer or floating point samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coding {
    Int,
    Float,
}

/// The fields of the `fmt ` chunk this decoder needs, resolved against
/// whichever of the three `fmt ` layouts the file used.
struct FmtInfo {
    coding: Coding,
    /// Bits that are meaningful within the container, e.g. 24 for a sample
    /// stored in a 4-byte container.
    bits_per_sample: u16,
    /// Bytes occupied by one sample, which can exceed `bits_per_sample / 8`.
    container_bytes: u16,
    num_channels: u16,
    sample_rate: u32,
}

/// Everything resolved from the header, before any frames are read.
struct Spec {
    coding: Coding,
    bits_per_sample: u16,
    container_bytes: u16,
    num_channels: usize,
    sample_rate: u32,
    /// `container_bytes * num_channels`, the byte stride of one frame.
    block_align: u64,
    /// Byte offset of the first sample in `data`.
    data_start: u64,
    num_frames: u64,
}

/// A WAV file whose header has been parsed and is ready to have frames read
/// from it.
pub(crate) struct OpenWav<R> {
    reader: R,
    pub(crate) num_channels: usize,
    pub(crate) sample_rate: u32,
    coding: Coding,
    bits_per_sample: u16,
    container_bytes: u16,
    block_align: u64,
    data_start: u64,
    num_frames: u64,
}

/// Attempts to open `reader` as a WAV file this decoder can read frames from.
///
/// Returns `Ok(None)` for anything this decoder does not handle: a file that
/// is not RIFF/WAVE, an unsupported sample encoding, or a header that ends
/// unexpectedly. The last case is deliberate - a header truncated mid-parse
/// is not different, as far as this fast path is concerned, from one that
/// was never valid, and the caller falls back to the general decoder either
/// way.
pub(crate) fn open_wav<R: Read + Seek>(mut reader: R) -> io::Result<Option<OpenWav<R>>> {
    match parse_header(&mut reader) {
        Ok(Some(spec)) => Ok(Some(OpenWav {
            reader,
            num_channels: spec.num_channels,
            sample_rate: spec.sample_rate,
            coding: spec.coding,
            bits_per_sample: spec.bits_per_sample,
            container_bytes: spec.container_bytes,
            block_align: spec.block_align,
            data_start: spec.data_start,
            num_frames: spec.num_frames,
        })),
        Ok(None) => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(err),
    }
}

/// Walks chunks until `data` is found, or a reason to give up is found first.
fn parse_header<R: Read + Seek>(reader: &mut R) -> io::Result<Option<Spec>> {
    let mut riff = [0u8; 12];
    reader.read_exact(&mut riff)?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Ok(None);
    }

    let mut fmt: Option<FmtInfo> = None;

    loop {
        let mut chunk_header = [0u8; 8];
        reader.read_exact(&mut chunk_header)?;
        let id = &chunk_header[0..4];
        let len = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap());

        if id == b"fmt " {
            fmt = match read_fmt_chunk(reader, len)? {
                Some(info) => Some(info),
                // An unsupported or malformed fmt chunk ends the attempt
                // outright, so the reader's position from here on does not
                // matter.
                None => return Ok(None),
            };
        } else if id == b"data" {
            // The fmt chunk must precede the data chunk; a file that gets
            // this backwards is not one we can make sense of here.
            let Some(fmt) = fmt else { return Ok(None) };

            let data_start = reader.stream_position()?;
            let file_len = reader.seek(SeekFrom::End(0))?;
            // A streaming writer can leave the data or RIFF size field wrong,
            // for example 0xFFFFFFFF when the final length was never patched
            // in. The actual file length is the only figure that cannot lie.
            let available = file_len.saturating_sub(data_start);
            let data_len = u64::from(len).min(available);
            let block_align = u64::from(fmt.container_bytes) * u64::from(fmt.num_channels);

            return Ok(Some(Spec {
                coding: fmt.coding,
                bits_per_sample: fmt.bits_per_sample,
                container_bytes: fmt.container_bytes,
                num_channels: usize::from(fmt.num_channels),
                sample_rate: fmt.sample_rate,
                block_align,
                data_start,
                // A trailing partial frame is dropped rather than treated as
                // an error: it is what a clamped or genuinely truncated
                // length produces, and one incomplete frame is not a reason
                // to refuse the rest of the file.
                num_frames: data_len / block_align,
            }));
        } else {
            // Any other chunk, including "fact", is skipped: this decoder
            // only handles PCM and IEEE float, for which the frame count is
            // fully determined by the data chunk length.
            skip_remaining(reader, len, 0)?;
        }
    }
}

/// Reads a `fmt ` chunk and resolves it to the fields this decoder needs.
///
/// Returns `None` for a format tag or bit depth this decoder does not
/// handle. On success, the reader is left positioned right after the whole
/// declared chunk, padding included, so the caller can keep walking chunks.
fn read_fmt_chunk<R: Read + Seek>(reader: &mut R, chunk_len: u32) -> io::Result<Option<FmtInfo>> {
    // A minimum chunk length of 16 is assumed for every `fmt ` layout: the
    // 14-byte `WAVEFORMAT` plus `wBitsPerSample`.
    if chunk_len < 16 {
        return Ok(None);
    }

    let mut base = [0u8; 16];
    reader.read_exact(&mut base)?;
    let format_tag = u16::from_le_bytes([base[0], base[1]]);
    let num_channels = u16::from_le_bytes([base[2], base[3]]);
    let sample_rate = u32::from_le_bytes([base[4], base[5], base[6], base[7]]);
    // base[8..12] is nAvgBytesPerSec, which is redundant with block_align and
    // sample_rate and is not needed to decode the data.
    let block_align = u16::from_le_bytes([base[12], base[13]]);
    let bits_per_sample = u16::from_le_bytes([base[14], base[15]]);

    let (coding, bits_per_sample, consumed) = match format_tag {
        WAVE_FORMAT_PCM => (Coding::Int, bits_per_sample, 16),
        WAVE_FORMAT_IEEE_FLOAT => (Coding::Float, bits_per_sample, 16),
        WAVE_FORMAT_EXTENSIBLE => {
            // 16 bytes were read already, plus 2 for cbSize, and cbSize
            // itself must be at least 22 for the rest of the extension, so
            // the chunk must be at least 40 bytes long.
            if chunk_len < 40 {
                return Ok(None);
            }
            // cbSize(2) + wValidBitsPerSample(2) + dwChannelMask(4) +
            // SubFormat(16).
            let mut ext = [0u8; 24];
            reader.read_exact(&mut ext)?;
            let valid_bits_per_sample = u16::from_le_bytes([ext[2], ext[3]]);
            let subformat: [u8; 16] = ext[8..24].try_into().unwrap();

            let coding = match subformat {
                SUBFORMAT_PCM => Coding::Int,
                SUBFORMAT_IEEE_FLOAT => Coding::Float,
                _ => return Ok(None),
            };
            // `wBitsPerSample` is what decides how the bytes are read;
            // `wValidBitsPerSample` only states how many of those bits carry
            // signal, and per the WAVEFORMATEXTENSIBLE definition the valid
            // bits are left-justified within the container. A 32-bit
            // container holding 24 valid bits is therefore read as a 32-bit
            // sample, and the normalized result is already correct because
            // the value sits in the high bits. Reading it as a 24-bit sample
            // instead would take the wrong bits entirely.
            //
            // A zero `wBitsPerSample` is the one case where the field cannot
            // be used and the valid-bits count is the only figure left. Such
            // files do occur; Symphonia rejects them outright, so accepting
            // them here only ever turns a failed read into a working one.
            let bits = if bits_per_sample == 0 {
                valid_bits_per_sample
            } else {
                bits_per_sample
            };
            (coding, bits, 40)
        }
        // ADPCM, A-law/mu-law and anything else are not decoded here.
        _ => return Ok(None),
    };

    let info = build_fmt_info(
        coding,
        bits_per_sample,
        block_align,
        num_channels,
        sample_rate,
    );
    if info.is_some() {
        skip_remaining(reader, chunk_len, consumed)?;
    }
    Ok(info)
}

/// Validates a resolved `fmt ` chunk and derives the container width from
/// `nBlockAlign`, which is what actually determines the byte stride, since
/// `nBlockAlign` can describe a container wider than `bits_per_sample`
/// implies (24-bit samples in a 4-byte container).
fn build_fmt_info(
    coding: Coding,
    bits_per_sample: u16,
    block_align: u16,
    num_channels: u16,
    sample_rate: u32,
) -> Option<FmtInfo> {
    // A zero sample rate cannot describe a timeline, and it divides by zero
    // as soon as a frame position or a resampling ratio is derived from it.
    if num_channels == 0
        || sample_rate == 0
        || block_align == 0
        || !block_align.is_multiple_of(num_channels)
    {
        return None;
    }
    let container_bytes = block_align / num_channels;

    let supported = match coding {
        Coding::Int => matches!(
            (bits_per_sample, container_bytes),
            (8, 1) | (16, 2) | (24, 3) | (24, 4) | (32, 4)
        ),
        Coding::Float => matches!((bits_per_sample, container_bytes), (32, 4) | (64, 8)),
    };
    if !supported {
        return None;
    }

    Some(FmtInfo {
        coding,
        bits_per_sample,
        container_bytes,
        num_channels,
        sample_rate,
    })
}

/// Skips whatever is left of a chunk whose header and `consumed` bytes of
/// body have already been read, including the pad byte that keeps chunk
/// bodies word-aligned. The pad is decided by the parity of the whole
/// declared length, not of the remainder, but `consumed` is always even in
/// every caller here, so the two agree.
fn skip_remaining<R: Read + Seek>(reader: &mut R, chunk_len: u32, consumed: u32) -> io::Result<()> {
    let pad = chunk_len & 1;
    // Every caller reads at most `chunk_len` bytes before this, so the
    // subtraction cannot go negative; saturating keeps a future caller that
    // gets it wrong from panicking on a malformed file.
    let skip = i64::from(chunk_len.saturating_sub(consumed)) + i64::from(pad);
    reader.seek(SeekFrom::Current(skip))?;
    Ok(())
}

/// Frames read per scratch buffer fill, bounded in bytes rather than frames
/// so that a file with many channels does not scale the buffer past a fixed
/// budget.
const BLOCK_BYTES: usize = 64 * 1024;

/// Reads `[start_frame, end_frame)` of `wav`, keeping only channels
/// `[channel_start, channel_start + channel_count)` of each frame.
///
/// The caller is expected to have already validated the frame range and
/// channel range; this only clamps the frame range to the file's length,
/// the same way the general decoder treats a range that runs past the end
/// of the file as yielding fewer frames rather than as an error.
pub(crate) fn read_frames<F: Float>(
    mut wav: OpenWav<impl Read + Seek>,
    start_frame: usize,
    end_frame: Option<usize>,
    channel_start: usize,
    channel_count: usize,
) -> io::Result<Vec<F>> {
    let total = wav.num_frames;
    let start = (start_frame as u64).min(total);
    let end = end_frame.map_or(total, |end| (end as u64).min(total));

    if start >= end {
        return Ok(Vec::new());
    }
    let frame_count = (end - start) as usize;

    let mut samples = Vec::with_capacity(
        frame_count
            .saturating_mul(channel_count)
            .min(MAX_PREALLOC_SAMPLES),
    );

    wav.reader
        .seek(SeekFrom::Start(wav.data_start + start * wav.block_align))?;

    let frame_bytes = wav.block_align as usize;
    let block_frames = (BLOCK_BYTES / frame_bytes).max(1);
    let mut buf = vec![0u8; block_frames * frame_bytes];

    let container = wav.container_bytes as usize;
    let channel_offset = channel_start * container;

    let mut remaining = frame_count;
    while remaining > 0 {
        let this_block = block_frames.min(remaining);
        let bytes = this_block * frame_bytes;
        wav.reader.read_exact(&mut buf[..bytes])?;

        for frame in buf[..bytes].chunks_exact(frame_bytes) {
            for ch in 0..channel_count {
                let offset = channel_offset + ch * container;
                let sample = decode_sample(
                    &frame[offset..offset + container],
                    wav.coding,
                    wav.bits_per_sample,
                );
                samples.push(F::from(sample).unwrap_or_else(F::zero));
            }
        }

        remaining -= this_block;
    }

    Ok(samples)
}

/// Converts one sample's raw bytes to a normalized `f64`.
///
/// `bytes.len()` is always `container_bytes` as resolved by
/// [`build_fmt_info`], so every combination reachable here was already
/// validated when the `fmt ` chunk was parsed.
fn decode_sample(bytes: &[u8], coding: Coding, bits_per_sample: u16) -> f64 {
    match (coding, bits_per_sample, bytes.len()) {
        // 8-bit PCM is the one width that is unsigned, with silence at 128
        // rather than 0.
        (Coding::Int, 8, 1) => (f64::from(bytes[0]) - 128.0) / 128.0,
        (Coding::Int, 16, 2) => f64::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0,
        (Coding::Int, 24, 3) => {
            let raw = u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16;
            f64::from(sign_extend_24(raw)) / 8_388_608.0
        }
        // The 24-bit sample occupies the low 3 bytes of the 4-byte
        // container; the top byte is not part of the value.
        (Coding::Int, 24, 4) => {
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) & 0x00ff_ffff;
            f64::from(sign_extend_24(raw)) / 8_388_608.0
        }
        (Coding::Int, 32, 4) => {
            f64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                / 2_147_483_648.0
        }
        (Coding::Float, 32, 4) => {
            f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        (Coding::Float, 64, 8) => f64::from_le_bytes(bytes.try_into().unwrap()),
        _ => unreachable!("validated when the fmt chunk was parsed"),
    }
}

/// Sign-extends a 24-bit value held in the low bits of a `u32`.
fn sign_extend_24(raw: u32) -> i32 {
    if raw & 0x0080_0000 == 0 {
        raw as i32
    } else {
        (raw | 0xff00_0000) as i32
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    use super::*;
    use crate::wav::encode;
    use crate::writer::SampleFormat;

    /// Opens an in-memory buffer, expecting the native decoder to accept it.
    fn open(bytes: &[u8]) -> OpenWav<Cursor<Vec<u8>>> {
        open_wav(Cursor::new(bytes.to_vec()))
            .expect("no io error reading from memory")
            .expect("should be accepted by the native decoder")
    }

    /// Opens an in-memory buffer, expecting the native decoder to defer to
    /// the general decoder instead.
    fn open_none(bytes: &[u8]) {
        assert!(
            open_wav(Cursor::new(bytes.to_vec()))
                .expect("no io error reading from memory")
                .is_none(),
            "should not be accepted by the native decoder"
        );
    }

    fn read_all<F: Float>(bytes: &[u8]) -> Vec<F> {
        let wav = open(bytes);
        let channels = wav.num_channels;
        read_frames(wav, 0, None, 0, channels).unwrap()
    }

    /// Builds a minimal RIFF/WAVE file from a `fmt ` chunk body and raw data
    /// bytes, with no chunks in between and no trailing bytes. Also exercises
    /// the RIFF padding rule for an odd-length `fmt ` chunk.
    fn build_wav(fmt_body: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // patched below
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt_body.len() as u32).to_le_bytes());
        out.extend_from_slice(fmt_body);
        if fmt_body.len() % 2 == 1 {
            out.push(0);
        }

        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }

        let riff_len = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&riff_len.to_le_bytes());
        out
    }

    /// A 16-byte `PCMWAVEFORMAT` fmt body.
    fn pcm_fmt_body(format_tag: u16, num_channels: u16, sample_rate: u32, bits: u16) -> Vec<u8> {
        let container_bytes = bits.div_ceil(8);
        let block_align = num_channels * container_bytes;
        let byte_rate = sample_rate * u32::from(block_align);

        let mut body = Vec::new();
        body.extend_from_slice(&format_tag.to_le_bytes());
        body.extend_from_slice(&num_channels.to_le_bytes());
        body.extend_from_slice(&sample_rate.to_le_bytes());
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&block_align.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body
    }

    /// A 40-byte `WAVEFORMATEXTENSIBLE` fmt body.
    fn extensible_fmt_body(
        num_channels: u16,
        sample_rate: u32,
        bits: u16,
        container_bytes: u16,
        valid_bits: u16,
        subformat: [u8; 16],
    ) -> Vec<u8> {
        let block_align = num_channels * container_bytes;
        let byte_rate = sample_rate * u32::from(block_align);

        let mut body = Vec::new();
        body.extend_from_slice(&0xfffeu16.to_le_bytes());
        body.extend_from_slice(&num_channels.to_le_bytes());
        body.extend_from_slice(&sample_rate.to_le_bytes());
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&block_align.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        body.extend_from_slice(&valid_bits.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // dwChannelMask
        body.extend_from_slice(&subformat);
        body
    }

    #[test]
    fn rejects_files_without_the_riff_wave_magic() {
        open_none(b"not a wav file at all, but 12+ bytes long");
    }

    /// Any truncation before the `data` chunk header is fully present leaves
    /// no reliable frame count to derive, so the whole attempt is abandoned.
    #[test]
    fn rejects_truncation_before_the_data_chunk_header_is_complete() {
        let data = [1u8, 0, 2, 0];
        let full = build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 16), &data);
        let data_header_end = full.len() - data.len();

        for len in 0..data_header_end {
            open_none(&full[..len]);
        }
    }

    /// Once the `data` chunk header is intact, a file cut short partway
    /// through the samples is treated the same as a declared length that
    /// overstates what is actually there: the available bytes are read
    /// instead of the file being rejected or read out of bounds.
    #[test]
    fn a_file_truncated_within_the_data_body_still_opens_with_the_available_frames() {
        let data: Vec<u8> = [1i16, 2, 3, 4]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let full = build_wav(&pcm_fmt_body(0x0001, 2, 44_100, 16), &data);
        let data_header_end = full.len() - data.len();

        // One full stereo frame (4 bytes) is present; the second frame's
        // first channel is cut off after 2 of its 4 bytes.
        let truncated = &full[..data_header_end + 6];

        let samples: Vec<f32> = read_all(truncated);
        assert_eq!(samples, [1.0 / 32_768.0, 2.0 / 32_768.0]);
    }

    #[test]
    fn rejects_unsupported_format_tags() {
        // ADPCM.
        open_none(&build_wav(&pcm_fmt_body(0x0002, 1, 44_100, 4), &[0, 0]));
    }

    /// A header describing something that cannot be indexed into must be
    /// declined rather than decoded, since every one of these divides by
    /// zero or produces a nonsense stride further down.
    #[test]
    fn rejects_degenerate_headers() {
        // Zero channels: the frame stride would be zero.
        let mut zero_channels = pcm_fmt_body(0x0001, 1, 44_100, 16);
        zero_channels[2..4].copy_from_slice(&0u16.to_le_bytes());
        open_none(&build_wav(&zero_channels, &[0, 0]));

        // Zero sample rate: no timeline, and a resample ratio would divide
        // by it.
        open_none(&build_wav(&pcm_fmt_body(0x0001, 1, 0, 16), &[0, 0]));

        // Zero nBlockAlign: no stride to advance by.
        let mut zero_align = pcm_fmt_body(0x0001, 1, 44_100, 16);
        zero_align[12..14].copy_from_slice(&0u16.to_le_bytes());
        open_none(&build_wav(&zero_align, &[0, 0]));

        // An nBlockAlign that is not a whole number of channels wide.
        let mut ragged = pcm_fmt_body(0x0001, 2, 44_100, 16);
        ragged[12..14].copy_from_slice(&5u16.to_le_bytes());
        open_none(&build_wav(&ragged, &[0, 0, 0, 0]));

        // A bit depth with no defined container layout.
        open_none(&build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 12), &[0, 0]));

        // A fmt chunk too short to hold even WAVEFORMAT.
        open_none(&build_wav(
            &pcm_fmt_body(0x0001, 1, 44_100, 16)[..14],
            &[0, 0],
        ));
    }

    /// Header fields come from an untrusted file, so no combination of them
    /// may panic, hang or read out of bounds. This walks a valid file and
    /// corrupts it one byte at a time across the whole header region, which
    /// reaches every length, count and tag the parser branches on.
    #[test]
    fn corrupted_headers_never_panic() {
        let base = build_wav(
            &extensible_fmt_body(2, 48_000, 16, 2, 16, SUBFORMAT_PCM),
            &[1, 0, 2, 0, 3, 0, 4, 0],
        );

        for offset in 0..base.len() {
            for patch in [0x00u8, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
                let mut bytes = base.clone();
                bytes[offset] = patch;

                // Either it is declined or it decodes, but it must not
                // panic and must not disagree with itself about how many
                // samples it produced.
                if let Some(wav) = open_wav(Cursor::new(bytes)).expect("no io error") {
                    let channels = wav.num_channels;
                    if let Ok(samples) = read_frames::<f64>(wav, 0, None, 0, channels) {
                        assert_eq!(
                            samples.len() % channels,
                            0,
                            "offset {offset} patch {patch:#04x} produced a partial frame"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_a_data_chunk_before_fmt() {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"data");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        let riff_len = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&riff_len.to_le_bytes());

        open_none(&out);
    }

    #[test]
    fn skips_unknown_chunks_between_fmt_and_data() {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        let fmt_body = pcm_fmt_body(0x0001, 1, 44_100, 16);
        out.extend_from_slice(&(fmt_body.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt_body);
        // Odd-length unknown chunk, to exercise the pad byte.
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&[9, 9, 9]);
        out.push(0); // pad
        out.extend_from_slice(b"data");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2i16.to_le_bytes());
        let riff_len = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&riff_len.to_le_bytes());

        let samples: Vec<f32> = read_all(&out);
        assert_eq!(samples, [2.0 / 32_768.0]);
    }

    #[test]
    fn eight_bit_pcm_is_unsigned_with_silence_at_128() {
        let bytes = build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 8), &[0xff, 0x80, 0x01]);
        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(samples, [127.0 / 128.0, 0.0, -127.0 / 128.0]);
    }

    #[test]
    fn sixteen_bit_pcm_round_trips_known_values() {
        let data: Vec<u8> = [2i16, -3, 5, -7]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let bytes = build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 16), &data);
        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(
            samples,
            [
                2.0 / 32_768.0,
                -3.0 / 32_768.0,
                5.0 / 32_768.0,
                -7.0 / 32_768.0
            ]
        );
    }

    #[test]
    fn thirty_two_bit_pcm_round_trips_known_values() {
        let data: Vec<u8> = [19i32, -229_373, 33_587_161, -2_147_483_497]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let bytes = build_wav(&pcm_fmt_body(0x0001, 2, 48_000, 32), &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(
            samples,
            [
                19.0 / 2_147_483_648.0,
                -229_373.0 / 2_147_483_648.0,
                33_587_161.0 / 2_147_483_648.0,
                -2_147_483_497.0 / 2_147_483_648.0,
            ]
        );
    }

    /// 24-bit samples in a plain 3-byte container, using the exact fixture
    /// values from the encoder's own sign-extension expectations.
    #[test]
    fn twenty_four_bit_pcm_in_a_three_byte_container_sign_extends() {
        // -17 and 8_388_607 (i24::MAX), little-endian, 3 bytes each.
        let data: Vec<u8> = vec![0xef, 0xff, 0xff, 0xff, 0xff, 0x7f];
        let bytes = build_wav(&pcm_fmt_body(0x0001, 1, 192_000, 24), &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(samples, [-17.0 / 8_388_608.0, 8_388_607.0 / 8_388_608.0]);
    }

    /// The nonstandard but common case of 24-bit samples in a 4-byte
    /// container, as produced by e.g. `arecord -f S24_LE`. `nBlockAlign`
    /// implies the 4-byte stride even though `wBitsPerSample` says 24.
    #[test]
    fn twenty_four_bit_pcm_in_a_four_byte_container() {
        let mut fmt = pcm_fmt_body(0x0001, 2, 48_000, 24);
        // Overwrite nBlockAlign (offset 12) to 8 (2 channels * 4 bytes) and
        // nAvgBytesPerSec (offset 8) to match.
        fmt[8..12].copy_from_slice(&(48_000u32 * 8).to_le_bytes());
        fmt[12..14].copy_from_slice(&8u16.to_le_bytes());

        // -96 and 23_052, little-endian, 4-byte containers (top byte unused).
        let data: Vec<u8> = vec![0xa0, 0xff, 0xff, 0x00, 0x0c, 0x5a, 0x00, 0x00];
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(samples, [-96.0 / 8_388_608.0, 23_052.0 / 8_388_608.0]);
    }

    #[test]
    fn thirty_two_bit_float_is_bit_exact() {
        let data: Vec<u8> = [2.0f32, 3.0, -16_411.0, 1_019.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let bytes = build_wav(&pcm_fmt_body(0x0003, 1, 44_100, 32), &data);
        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(samples, [2.0, 3.0, -16_411.0, 1_019.0]);
    }

    #[test]
    fn sixty_four_bit_float_is_bit_exact() {
        let data: Vec<u8> = [1.5f64, -2.5]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let bytes = build_wav(&pcm_fmt_body(0x0003, 1, 44_100, 64), &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(samples, [1.5, -2.5]);
    }

    #[test]
    fn extensible_pcm_is_read_via_the_subformat_guid() {
        let fmt = extensible_fmt_body(1, 192_000, 24, 3, 24, SUBFORMAT_PCM);
        let data: Vec<u8> = vec![0xef, 0xff, 0xff]; // -17
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(samples, [-17.0 / 8_388_608.0]);
    }

    #[test]
    fn extensible_float_is_read_via_the_subformat_guid() {
        let fmt = extensible_fmt_body(1, 44_100, 32, 4, 32, SUBFORMAT_IEEE_FLOAT);
        let data: Vec<u8> = 2.5f32.to_le_bytes().to_vec();
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(samples, [2.5]);
    }

    /// A zero `wValidBitsPerSample`, which occurs in files in the wild, says
    /// nothing about how the bytes are laid out, so `wBitsPerSample` decides
    /// as it always does.
    #[test]
    fn extensible_zero_valid_bits_is_ignored() {
        let fmt = extensible_fmt_body(2, 48_000, 32, 4, 0, SUBFORMAT_PCM);
        let data: Vec<u8> = [19i32, -229_373]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(
            samples,
            [19.0 / 2_147_483_648.0, -229_373.0 / 2_147_483_648.0]
        );
    }

    /// `wValidBitsPerSample` below the container width does not change how
    /// the bytes are read: the valid bits are left-justified in the
    /// container, so reading the full container is already correct. Reading
    /// the low `wValidBitsPerSample` bits instead would take the wrong bits.
    #[test]
    fn extensible_valid_bits_below_the_container_does_not_narrow_the_read() {
        let fmt = extensible_fmt_body(1, 48_000, 32, 4, 24, SUBFORMAT_PCM);
        let data: Vec<u8> = 33_587_161i32.to_le_bytes().to_vec();
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f64> = read_all(&bytes);

        assert_eq!(samples, [33_587_161.0 / 2_147_483_648.0]);
        // The wrong reading would take the low 24 bits and scale by 2^23,
        // which is a different value entirely rather than a rounding
        // difference.
        let wrong = f64::from(33_587_161i32 & 0x00ff_ffff) / 8_388_608.0;
        assert_ne!(samples[0], wrong);
    }

    /// The only case where `wValidBitsPerSample` is consulted: a zero
    /// `wBitsPerSample` leaves nothing else to derive the sample width from.
    /// Symphonia rejects such files, so reading them here can only turn a
    /// failed read into a working one.
    #[test]
    fn extensible_zero_bits_per_sample_falls_back_to_valid_bits() {
        let fmt = extensible_fmt_body(1, 48_000, 0, 2, 16, SUBFORMAT_PCM);
        let data: Vec<u8> = (-3i16).to_le_bytes().to_vec();
        let bytes = build_wav(&fmt, &data);
        let samples: Vec<f64> = read_all(&bytes);
        assert_eq!(samples, [-3.0 / 32_768.0]);
    }

    #[test]
    fn extensible_with_an_unknown_subformat_guid_is_unsupported() {
        let mut unknown = SUBFORMAT_PCM;
        unknown[0] = 0xaa;
        let fmt = extensible_fmt_body(2, 48_000, 16, 2, 16, unknown);
        open_none(&build_wav(&fmt, &[0, 0, 0, 0]));
    }

    #[test]
    fn extensible_chunk_shorter_than_forty_bytes_is_unsupported() {
        let mut fmt = extensible_fmt_body(1, 44_100, 16, 2, 16, SUBFORMAT_PCM);
        fmt.truncate(39);
        open_none(&build_wav(&fmt, &[0, 0]));
    }

    /// A streaming writer can leave the data chunk's declared length far
    /// beyond what the file actually contains (a common convention is
    /// `0xFFFFFFFF`). The available bytes should be used instead of failing
    /// or reading past the end of the buffer.
    #[test]
    fn declared_data_length_is_clamped_to_the_actual_file_size() {
        let mut bytes = build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 16), &[1, 0, 2, 0, 3, 0]);
        // Overwrite the data chunk's length field with a value the file
        // cannot possibly hold.
        let data_len_pos = bytes.len() - 6 - 4;
        bytes[data_len_pos..data_len_pos + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(samples.len(), 3, "only the real data is readable");
    }

    /// A trailing partial frame from a clamped or truncated length is
    /// dropped rather than causing an error or reading out of bounds.
    #[test]
    fn a_trailing_partial_frame_is_dropped() {
        // Two channels of 16-bit samples is a 4-byte frame; five bytes is
        // one full frame plus one stray byte.
        let bytes = build_wav(&pcm_fmt_body(0x0001, 2, 44_100, 16), &[1, 0, 2, 0, 0xff]);
        let samples: Vec<f32> = read_all(&bytes);
        assert_eq!(
            samples.len(),
            2,
            "one stereo frame, the stray byte is dropped"
        );
    }

    /// Selecting a frame range must match the corresponding slice of a full
    /// decode, including across multiple internal block reads.
    #[test]
    fn frame_range_selection_matches_a_slice_of_the_full_decode() {
        const CHANNELS: usize = 3;
        const FRAMES: usize = 5_000; // spans several internal read blocks

        let mut samples = Vec::with_capacity(FRAMES * CHANNELS);
        for frame in 0..FRAMES {
            for ch in 0..CHANNELS {
                // A value that uniquely identifies (frame, channel) and is
                // exactly representable in f32.
                samples.push((frame * CHANNELS + ch) as f32 / (FRAMES * CHANNELS) as f32 - 0.5);
            }
        }

        let layout = encode::Layout::new(
            samples.len(),
            CHANNELS as u16,
            48_000,
            SampleFormat::Float32,
        )
        .unwrap();
        let mut bytes = Vec::new();
        encode::write(&mut bytes, &layout, &samples).unwrap();

        let full: Vec<f32> = read_all(&bytes);
        assert_eq!(full, samples, "sanity check: full decode matches the input");

        for (start, end) in [
            (0, Some(1)),
            (10, Some(20)),
            (100, Some(4_500)),
            (4_999, Some(FRAMES)),
            (0, None),
            (FRAMES, Some(FRAMES + 100)),
            (FRAMES + 5, None),
        ] {
            let wav = open(&bytes);
            let got: Vec<f32> = read_frames(wav, start, end, 0, CHANNELS).unwrap();

            let clamped_end = end.unwrap_or(FRAMES).min(FRAMES);
            let clamped_start = start.min(FRAMES);
            let expected = if clamped_start >= clamped_end {
                &[][..]
            } else {
                &samples[clamped_start * CHANNELS..clamped_end * CHANNELS]
            };
            assert_eq!(got, expected, "start={start} end={end:?}");
        }
    }

    /// Selecting a channel range must match the corresponding channels of
    /// every frame in a full decode.
    #[test]
    fn channel_range_selection_matches_the_full_decode() {
        const CHANNELS: usize = 6;
        const FRAMES: usize = 50;

        let mut samples = Vec::with_capacity(FRAMES * CHANNELS);
        for frame in 0..FRAMES {
            for ch in 0..CHANNELS {
                samples.push((frame * CHANNELS + ch) as f32);
            }
        }

        let layout = encode::Layout::new(
            samples.len(),
            CHANNELS as u16,
            44_100,
            SampleFormat::Float32,
        )
        .unwrap();
        let mut bytes = Vec::new();
        encode::write(&mut bytes, &layout, &samples).unwrap();

        for (start, count) in [(0, 6), (0, 1), (1, 2), (5, 1), (2, 4)] {
            let wav = open(&bytes);
            let got: Vec<f32> = read_frames(wav, 0, None, start, count).unwrap();

            let expected: Vec<f32> = (0..FRAMES)
                .flat_map(|frame| {
                    (start..start + count).map(move |ch| (frame * CHANNELS + ch) as f32)
                })
                .collect();
            assert_eq!(got, expected, "start={start} count={count}");
        }
    }

    #[test]
    fn start_frame_beyond_the_file_yields_no_samples() {
        let bytes = build_wav(&pcm_fmt_body(0x0001, 1, 44_100, 16), &2i16.to_le_bytes());
        let samples: Vec<f32> = {
            let wav = open(&bytes);
            read_frames(wav, 100, None, 0, 1).unwrap()
        };
        assert!(samples.is_empty());
    }

    #[test]
    fn an_empty_data_chunk_yields_no_samples_and_no_error() {
        let bytes = build_wav(&pcm_fmt_body(0x0001, 2, 48_000, 16), &[]);
        let samples: Vec<f32> = read_all(&bytes);
        assert!(samples.is_empty());
    }

    /// A reader that records how many bytes were pulled out of it, so that a
    /// test can assert on what a read cost and not only on what it returned.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        bytes_read: Rc<Cell<u64>>,
    }

    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read.set(self.bytes_read.get() + n as u64);
            Ok(n)
        }
    }

    impl Seek for Counting {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// The whole point of the fast path: what a read costs is set by the range
    /// asked for and not by the length of the file in front of it. Checking
    /// the returned samples cannot tell a decoder that seeks apart from one
    /// that walks the whole file, so this counts the bytes that were actually
    /// pulled out of the reader.
    #[test]
    fn reading_a_sub_range_reads_only_that_range() {
        const CHANNELS: usize = 2;
        const FRAMES: usize = 200_000;
        const FRAME_BYTES: usize = CHANNELS * 4;
        const WANTED: usize = 10;

        let samples = vec![0.0f32; FRAMES * CHANNELS];
        let layout = encode::Layout::new(
            samples.len(),
            CHANNELS as u16,
            48_000,
            SampleFormat::Float32,
        )
        .unwrap();
        let mut bytes = Vec::new();
        encode::write(&mut bytes, &layout, &samples).unwrap();
        assert!(
            bytes.len() > 1_000_000,
            "the file has to dwarf the range for this to prove anything"
        );

        let counter = Rc::new(Cell::new(0));
        let wav = open_wav(Counting {
            inner: Cursor::new(bytes),
            bytes_read: Rc::clone(&counter),
        })
        .expect("no io error reading from memory")
        .expect("should be accepted by the native decoder");

        // Parsing the header walks chunk headers and the fmt body, and seeks
        // over everything else.
        let header_bytes = counter.get();
        assert!(header_bytes < 128, "the header cost {header_bytes} bytes");

        // Late in the file, so that a decoder reading from the start would
        // have to pull almost all of it.
        let got: Vec<f32> = read_frames(wav, FRAMES - WANTED, Some(FRAMES), 0, CHANNELS).unwrap();
        assert_eq!(got.len(), WANTED * CHANNELS);

        assert_eq!(
            counter.get() - header_bytes,
            (WANTED * FRAME_BYTES) as u64,
            "exactly the requested frames and not one byte more"
        );
    }

    /// The decoder reads `nChannels` and never the speaker mask, so the channel
    /// count is bounded only by what `nBlockAlign` can describe. This is well
    /// past the ceiling any reader that maps channels onto named speakers has.
    #[test]
    fn a_channel_count_far_past_any_speaker_layout_is_read() {
        const CHANNELS: usize = 300;
        const FRAMES: usize = 4;

        let mut data = Vec::new();
        for frame in 0..FRAMES {
            for ch in 0..CHANNELS {
                data.extend_from_slice(&((frame * CHANNELS + ch) as i16).to_le_bytes());
            }
        }
        let fmt = extensible_fmt_body(CHANNELS as u16, 48_000, 16, 2, 16, SUBFORMAT_PCM);
        let bytes = build_wav(&fmt, &data);

        assert_eq!(open(&bytes).num_channels, CHANNELS);

        // A frame range and a channel range out of the far end of such a frame,
        // which is where a stride computed from anything but nChannels breaks.
        let got: Vec<f64> = read_frames(open(&bytes), 1, Some(3), CHANNELS - 3, 3).unwrap();
        let expected: Vec<f64> = (1..3)
            .flat_map(|frame| {
                (CHANNELS - 3..CHANNELS).map(move |ch| (frame * CHANNELS + ch) as f64 / 32_768.0)
            })
            .collect();
        assert_eq!(got, expected);
    }
}
