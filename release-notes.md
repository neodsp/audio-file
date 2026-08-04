## Breaking Changes

- Upgraded to `symphonia` 0.6 and `rubato` 4. Both are part of the public API,
  through `ReadError::Decode`, the `ResampleError` variants, and the
  `rubato::Sample` bound on `read`, `read_block`, and `resample`, so callers
  have to move to the same major versions.
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
  timestamps, instead of failing the whole read.
- Support for chained streams (e.g. concatenated OGG files): the decoder is
  rebuilt when the track list changes, and the timeline continues across
  streams.
- The sample rate is checked against every decoded packet, and against the track
  list of a chained stream. A file that switches its sample rate mid-stream now
  fails with `SampleRateChanged` instead of returning audio that plays at the
  wrong rate.
- Integer sample conversion when writing now rounds to the nearest integer
  instead of truncating towards zero, and clamps to the symmetric range
  `[-max, max]`. This also fixes full-scale input turning into silence when
  writing `Int32` from `f32` samples, where the scale factor rounded up out of
  the `i32` range.
- Channel selection is validated up front, so an invalid selection is rejected
  even for files that contain no audio packets.
- Files without audio frames now report the channel layout declared by the
  container instead of failing.
- The output buffer is reserved up front from the frame count reported by the
  container, so a long read no longer reallocates repeatedly while decoding.
- The documentation now states that `start` is inclusive and `stop` is
  exclusive, so reading from frame 300 to frame 400 yields 100 frames. This has
  always been the behavior.
- Dependency housekeeping: the internal `audioadapter-buffers` dependency moved
  to 4, and `approx` moved to the dev-dependencies. Neither is part of the
  public API.

## New Error Variants

- `ReadError`: `NoChannels`, `TooManyChannels`, `InvalidChannelRange`,
  `ChannelCountChanged`, `SampleRateChanged`
- `WriteError`: `ZeroChannels` and `UnalignedSamples`. Writing with zero
  channels or a sample count that is not a multiple of the channel count is now
  rejected before the file is created.
- `ResampleError`: `ZeroChannels`

## Fixes

- A `stop` position no longer bypasses resampling when a target sample rate is
  set.
- Channel selection no longer overflows on out-of-range input. Defaulting the
  channel count for a `start_channel` beyond the channel count of the file
  underflowed, and validating a `num_channels` close to `usize::MAX` overflowed.
  Both are now reported as an error.
- The `InvalidFrameRange` error message now correctly states that the start
  frame must not exceed the end frame.
- Resampling an empty selection no longer fails.
