# 0.5.0

Wav is now handled by this crate itself, in both directions.

## Highlights

- **Built-in wav reader.** Integer PCM and IEEE float wav, which is everything
  this crate writes, no longer goes through Symphonia. It is also faster: a short
  selection out of a long file reads only the bytes it needs. A-law, mu-law and
  ADPCM still fall back to Symphonia.
- **Built-in wav writer.** `hound` is gone. Chunk sizes are resolved before
  anything is written, write errors surface from `write` instead of on drop, and a
  failed write cleans up its truncated file.
- **Symphonia and `rubato` are optional.** Both on by default. Turn them off and a
  build that writes wav and reads wav has 8 crates in its tree instead of 46.
- **A file is read in full or not at all.** A packet the decoder rejects is now an
  error instead of being silently skipped. Sample rate and channel layout come
  from the decoded audio, not from container metadata that may contradict it.
- **Frame-accurate reads.** Positions come from packet timestamps rather than
  counted frames, and MP3-style encoder delay is excluded, so frame 0 is the first
  playable frame.

## Fixed

- Mono float and 32-bit wav files no longer play through the left speaker alone.
- Multichannel files no longer claim a speaker layout nobody gave them.
- Wav with 19+ channels can be read back again; there is no channel ceiling now.
- Writing integer samples rounds instead of truncating, and full-scale `f32` no
  longer turns into silence as `Int32`.
- A `stop` position no longer bypasses resampling.

## Breaking Changes

- `symphonia` 0.6 and `rubato` 4, both in the public API.
- `hound` is gone, so `WriteError::Encode` is gone. I/O errors are `WriteError::Io`.
- The `resample` module is private. `ResampleError` moved to `audio_file::ResampleError`.
- Error enums are `#[non_exhaustive]`, so a `match` needs a wildcard arm.
- `ReadError::InvalidChannel` → `InvalidStartChannel`. `InvalidChannelCount` split
  into `ZeroChannels` and `InvalidChannelRange`.
- The `wav` feature is replaced by `wav-compressed`, which actually includes the
  codecs. The old flag was the demuxer alone and could decode nothing.
- Time positions round to the nearest frame instead of truncating.
- Multi-track files pick the container's default audio track first.
- Several new error variants across `ReadError` and `WriteError`, mostly for
  invalid write arguments and wav's 32-bit size limits.
