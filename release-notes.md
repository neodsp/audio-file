## Breaking Changes

- Upgraded to `symphonia` 0.6 and `rubato` 4. Both are part of the public API,
  through `ReadError::Decode`, the `ResampleError` variants, and the
  `rubato::Sample` bound on `read` and `read_block`, so callers have to move to
  the same major versions.
- Time-based `start`/`stop` positions are now rounded to the nearest frame
  instead of being truncated. A selection can therefore shift by one frame
  compared to 0.4.x.
- The audio track is now selected by trying the container's default audio track
  first, then falling back through the remaining audio tracks until a decoder
  can be constructed. Null, unsupported, and otherwise unusable codecs are
  skipped. Previously the first track with a non-null codec was used, so a file
  with several audio tracks may now read a different one.
- Two `ReadError` variants were renamed, for consistency with the rest of the
  crate:
  - `InvalidChannel { index, total }` is now
    `InvalidStartChannel { start, total }`.
  - `InvalidChannelCount(usize)` is split in two. A zero channel count now
    returns `ZeroChannels`, matching `WriteError::ZeroChannels` and
    `ResampleError::ZeroChannels`, and a channel range that exceeds the channel
    count of the file returns the new `InvalidChannelRange` variant, which also
    reports the offending range.
- `ReadError`, `WriteError`, and `ResampleError` are now `#[non_exhaustive]`, so
  a `match` over them needs a wildcard arm. In exchange, variants added in later
  releases are no longer breaking changes.
- `hound` is no longer a dependency. Wav files are encoded by this crate, so
  `WriteError::Encode(hound::Error)` is gone and I/O failures are reported as
  `WriteError::Io(std::io::Error)` instead.
- The `resample` module is no longer public. Resampling is an implementation
  detail of the `sample_rate` read option, and standalone resampling is better
  served by `rubato` directly. `ResampleError` stays public because
  `ReadError::Resample` carries it, but its path changed from
  `audio_file::resample::ResampleError` to `audio_file::ResampleError`.
- `symphonia` is now an optional dependency, so `ReadError::Decode`, which
  carries a `symphonia` error, exists only when it is enabled. Every codec
  feature enables it and the default feature set enables all of them, so this
  only affects builds with default features off. See below for what such a build
  can still do.

## Improvements

- Frame positions now account for encoder delay and padding, as used by formats
  like MP3. The trimmed frames are excluded from the timeline, so frame 0 is the
  first playable frame.
- Reading is now positioned by packet timestamps instead of counting decoded
  frames, which makes frame-accurate reads robust against decoders that return
  fewer frames than a packet covers (e.g. while warming up after a seek).
- Seeking for large start offsets now aims one second early instead of 10%
  early, giving codecs with inter-frame dependencies a fixed amount of time to
  warm up.
- Malformed packets are skipped when the stream position can be recovered from
  timestamps, instead of failing the whole read. The frames such a packet would
  have carried are filled with silence, so that every later frame stays at its
  own position instead of moving earlier by the number of missing frames.
- Support for chained streams (e.g. concatenated OGG files): the decoder is
  rebuilt when the track list changes, and the timeline continues across
  streams.
- The sample rate is taken from the decoded audio, which is authoritative,
  instead of from a container declaration that may contradict the bitstream
  headers. A Matroska file whose `SamplingFrequency` element disagrees with the
  FLAC stream info is no longer reported with the wrong sample rate, and
  time-based positions as well as the resampling ratio are resolved against the
  rate the file really has.
- The sample rate is checked against every decoded packet, and against the track
  list of a chained stream. A file that switches its sample rate mid-stream now
  fails with `SampleRateChanged` instead of returning audio that plays at the
  wrong rate.
- Integer sample conversion when writing now rounds to the nearest integer
  instead of truncating towards zero, and clamps to the symmetric range
  `[-max, max]`. This also fixes full-scale input turning into silence when
  writing `Int32` from `f32` samples, where the scale factor rounded up out of
  the `i32` range.
- Channel selection is validated against the decoded channel layout, which is
  authoritative, instead of against a possibly stale container declaration. For
  files without any decoded audio the declared layout is still used, so an
  invalid selection is rejected even then.
- Files without audio frames now report the channel layout declared by the
  container instead of failing.
- The output buffer is reserved up front from the frame count reported by the
  container, so a long read no longer reallocates repeatedly while decoding.
- The documentation now states that `start` is inclusive and `stop` is
  exclusive, so reading from frame 300 to frame 400 yields 100 frames. This has
  always been the behavior.
- Wav is now handled by this crate on both sides, without a third party crate in
  either direction. Encoding no longer goes through `hound`, which the speaker
  assignment fixes below needed, and reading no longer goes through Symphonia for
  the encodings this crate itself writes.
- On the encoding side, every chunk size is resolved before the first byte is
  written, so the encoder never seeks back over its own output, and a write error
  surfaces from `write` instead of being discovered while a buffer is flushed on
  drop.
- On the decoding side, integer PCM and IEEE float wav files, 8/16/24/32-bit
  integer (including 24 bits in a 4-byte container) and 32/64-bit float, are read
  by a native decoder. PCM in a wav file is a flat byte array, so a frame range
  and a channel range are resolved by indexing into it, with none of the packet
  timestamps, decoder warm-up and seek verification a compressed format needs.
  Only the requested bytes are read, so a short selection out of a long file no
  longer decodes everything before it. Anything the native decoder does not cover
  - ADPCM, A-law/mu-law, an unrecognised format tag, or a file that is not
  RIFF/WAVE - falls back to Symphonia, so no file that used to be readable
  stopped being readable.
- Wav files with high channel counts can now be read back. Symphonia derives the
  channel layout from the extensible `dwChannelMask` and only knows 18 standard
  speaker positions, so it rejected a mask naming more than that, and files this
  crate wrote with 19 or more channels could not be read by this crate. The
  native decoder reads `nChannels` and never looks at the mask, because this
  crate reports a channel count and not a speaker layout, so there is no channel
  ceiling on wav anymore. Reading 64 channels back out of a file this crate wrote
  is now a test. The ceilings on the other containers are unchanged and still
  documented under Known Limitations.
- Dependency housekeeping: `hound` is gone, the internal `audioadapter-buffers`
  dependency moved to 4, and `approx` moved to the dev-dependencies. None of them
  is part of the public API.
- `symphonia` is optional. Wav files holding integer PCM or IEEE float samples,
  which is everything this crate writes, are read by the built-in decoder, so a
  build with default features off writes wav, reads wav, resamples, and pulls no
  `symphonia` at all. That is 23 crates in the dependency tree instead of 46.
  Every codec feature (`mp3`, `flac`, `mkv`, ...) enables it, so nothing changes
  for anyone who names a format. `simd` is now a modifier rather than an enabler
  and does nothing on its own in a build without `symphonia`.
- The `wav` feature is replaced by `wav-compressed`, which adds the wav encodings the
  built-in decoder does not cover: A-law, mu-law and ADPCM. Symphonia splits the
  RIFF demuxer from the encodings inside it, so the old `wav` feature was the
  demuxer alone and could decode none of them. Enabling it pulled all of
  Symphonia in while widening what could be read by nothing at all: integer PCM
  and IEEE float now read with no feature, and an A-law file still failed for
  want of a codec. `wav-compressed` is the demuxer plus `pcm` and `adpcm`, which is
  what reading those files actually takes. The supported format table now also
  says which wav encodings need no feature.
- `num` is replaced by `num-traits`, which is the only part of it this crate ever
  used. The `num` facade pulled in `num-bigint`, `num-rational` and `num-iter`
  for nothing. `num::Float` is a re-export of `num_traits::Float`, the same trait
  from the same crate, so the bound on `read`, `read_block` and `write` is
  unchanged and code written against `num::Float` still compiles.

## New Error Variants

- `ReadError`: `NoChannels`, `TooManyChannels`, `InvalidChannelRange`,
  `ChannelCountChanged`, `SampleRateChanged`
- `ReadError`: `UnsupportedFormat`, for a file no decoder in the build can read.
  Only reachable without the `symphonia` feature, where the built-in wav decoder
  is the whole reader and anything it declines has nowhere left to go.
- `WriteError`: `ZeroChannels`, `ZeroSampleRate`, and `UnalignedSamples`.
  Writing with zero channels, a zero sample rate, or a sample count that is not
  a multiple of the channel count is now rejected before the file is created. A
  zero sample rate used to produce a file with no timeline, which this crate
  could not read back through its own wav decoder.
- `WriteError`: `FileTooLarge`, `FrameTooLarge`, and `ByteRateTooHigh`. Wav
  describes its sizes in 32-bit and 16-bit fields, which caps a file at 4 GiB, a
  frame at 65535 bytes, and the byte rate at what `nAvgBytesPerSec` can hold.
  Output beyond any of those is now rejected before the file is created, instead
  of being written with wrapped size fields.
- `ResampleError`: `ZeroChannels`

## Fixes

- Mono files are no longer assigned to a single physical speaker. Anything wider
  than 16 bits per sample used to be written as a `WAVEFORMATEXTENSIBLE`, whose
  `dwChannelMask` named `SPEAKER_FRONT_LEFT` when there was one channel, so a
  mono `Float32` or `Int32` file played through the left speaker alone on players
  that honor the mask. Mono and stereo now use `PCMWAVEFORMAT` for integer
  samples and `WAVEFORMATEX` for float, neither of which carries a mask. That is
  also what ffmpeg and libsndfile write, and the mono float header this crate
  produces is now byte for byte identical to ffmpeg's.
- Multichannel files no longer claim a speaker layout they were never given.
  `dwChannelMask` used to be filled with one bit per channel, which labels the
  fourth channel of a quadraphonic file as the subwoofer feed. It is now zero,
  meaning the channels are not assigned to physical speakers, because the channel
  count is all this crate is told.
- A data chunk of an odd length is now followed by the pad byte that RIFF
  requires, so a file with an odd number of `Int8` samples is word aligned like
  every other chunk in the format.
- A `stop` position no longer bypasses resampling when a target sample rate is
  set.
- Channel selection no longer overflows on out-of-range input. Defaulting the
  channel count for a `start_channel` beyond the channel count of the file
  underflowed, and validating a `num_channels` close to `usize::MAX` overflowed.
  Both are now reported as an error.
- The `InvalidFrameRange` error message now correctly states that the start
  frame must not exceed the end frame.
- Resampling an empty selection no longer fails.
- A write that fails halfway, for example because the device is full, now removes
  the truncated file instead of leaving it behind to be mistaken for a finished
  one.
