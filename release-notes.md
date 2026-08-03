## Breaking Changes

- Upgraded to `symphonia` 0.6, `rubato` 4, and `audioadapter-buffers` 4.
- Time-based `start`/`stop` positions are now rounded to the nearest frame
  instead of being truncated. A selection can therefore shift by one frame
  compared to 0.4.x.
- Requesting a channel range that exceeds the channel count of the file now
  returns the new `ReadError::InvalidChannelRange` variant instead of
  `ReadError::InvalidChannelCount`, and also reports the offending range.
- `approx` is no longer a public dependency; it is only used in tests.

## Improvements

- Frame positions now refer to playable audio: encoder delay and padding, as
  used by formats like MP3, are trimmed, so frame 0 is the first playable
  frame. The `stop` position is exclusive.
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
- Integer sample conversion when writing now rounds to the nearest integer and
  clamps to the symmetric range `[-max, max]`, so full-scale input neither
  wraps around nor drops out.
- Channel selection is validated up front, so an invalid selection is rejected
  even for files that contain no audio packets.
- Files without audio frames now report the channel layout declared by the
  container instead of failing.

## New Error Variants

- `ReadError`: `NoChannels`, `InvalidChannelRange`, `ChannelCountChanged`,
  `SampleRateChanged`
- `WriteError`: `ZeroChannels`, `UnalignedSamples`: writing with zero channels
  or a sample count that is not a multiple of the channel count is now rejected
  before the file is created.
- `ResampleError`: `ZeroChannels`

## Fixes

- A `stop` position no longer bypasses resampling when a target sample rate is
  set.
- A `start_channel` beyond the channel count of the file no longer overflows
  while defaulting the channel count to "all remaining channels".
- The `InvalidFrameRange` error message now correctly states that the start
  frame must not exceed the end frame.
- Resampling an empty selection no longer fails.
