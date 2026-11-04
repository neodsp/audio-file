# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] - 2026-11-04

### Changed

- Tested against the latest dependency releases: `audioadapter-buffers` 5.2.0,
  `rubato` 5.0.1, `symphonia` 0.6.1 and `thiserror` 2.0.21. The minimum versions
  this crate requires are unchanged.
- Releases are published to crates.io with trusted publishing, so no API token is
  stored in the repository.
- The GitHub release notes are taken from this changelog.
- CI also checks for unused dependencies and that the crate packages cleanly, and
  treats compiler warnings as errors on macOS and Windows.

## [0.5.0] - 2026-08-25

Wav is now handled by this crate itself, in both directions.

### Added

- **Built-in wav reader.** Integer PCM and IEEE float wav, which is everything
  this crate writes, no longer goes through Symphonia. It is also faster: a short
  selection out of a long file reads only the bytes it needs. A-law, mu-law and
  ADPCM still fall back to Symphonia.
- **Built-in wav writer.** Chunk sizes are resolved before anything is written,
  write errors surface from `write` instead of on drop, and a failed write cleans
  up its truncated file.
- Several new error variants across `ReadError` and `WriteError`, mostly for
  invalid write arguments and wav's 32-bit size limits.

### Changed

- **Symphonia and `rubato` are optional.** Both on by default. Turn them off and a
  build that writes wav and reads wav has 8 crates in its tree instead of 44.
- **A file is read in full or not at all.** A packet the decoder rejects is now an
  error instead of being silently skipped. Sample rate and channel layout come
  from the decoded audio, not from container metadata that may contradict it.
- **Frame-accurate reads.** Positions come from packet timestamps rather than
  counted frames, and MP3-style encoder delay is excluded, so frame 0 is the first
  playable frame.
- **Breaking:** `symphonia` 0.6 and `rubato` 5, both in the public API.
- **Breaking:** The `resample` module is private. `ResampleError` moved to
  `audio_file::ResampleError`.
- **Breaking:** Error enums are `#[non_exhaustive]`, so a `match` needs a wildcard
  arm.
- **Breaking:** `ReadError::InvalidChannel` → `InvalidStartChannel`.
  `InvalidChannelCount` split into `ZeroChannels` and `InvalidChannelRange`.
- **Breaking:** The `wav` feature is replaced by `wav-compressed`, which actually
  includes the codecs. The old flag was the demuxer alone and could decode
  nothing.
- **Breaking:** Time positions round to the nearest frame instead of truncating.
- **Breaking:** Multi-track files pick the container's default audio track first.

### Removed

- **Breaking:** `hound` is gone, so `WriteError::Encode` is gone. I/O errors are
  `WriteError::Io`.

### Fixed

- Mono float and 32-bit wav files no longer play through the left speaker alone.
- Multichannel files no longer claim a speaker layout nobody gave them.
- Wav with 19+ channels can be read back again; there is no channel ceiling now.
- Writing integer samples rounds instead of truncating, and full-scale `f32` no
  longer turns into silence as `Int32`.
- A `stop` position no longer bypasses resampling.

## [0.4.1] - 2026-03-20

### Changed

- Skip resampling when the file already is in the selected sampling rate.

## [0.4.0] - 2026-02-26

**This is a breaking release.** The library has been renamed and all public APIs
simplified.

### Added

- `Int8` (8-bit integer) and `Int32` (32-bit integer) write support.
  `SampleFormat` now supports `Int8`, `Int16` (default), `Int32` and `Float32`.
- `ReadError::Resample`, so resample errors are propagated instead of being
  mapped to `NoTrack`.
- The README is generated from the `lib.rs` doc comments via `cargo rdme`, and CI
  checks that it stays in sync. All code examples are verified as doctests.

### Changed

- **Breaking:** Renamed from `audio-io` to `audio-file`: the crate name, package
  name and repository URL have all changed.
- **Breaking:** Public types and functions drop the `Audio` / `audio_` prefix,
  since they are already namespaced under `audio_file::`:
  - `audio_read()` → `read()`
  - `audio_write()` → `write()`
  - `audio_read_block()` → `read_block()`
  - `audio_write_block()` → `write_block()`
  - `AudioReadConfig` → `ReadConfig`
  - `AudioWriteConfig` → `WriteConfig`
  - `AudioReadError` → `ReadError`
  - `AudioWriteError` → `WriteError`
  - `WriteSampleFormat` → `SampleFormat`
- **Breaking:** Error variants have clearer names and structured fields:
  - `ReadError::FileError` → `ReadError::Io`
  - `ReadError::EncodingError` → `ReadError::Decode`
  - `ReadError::EndFrameLargerThanStartFrame` →
    `ReadError::InvalidFrameRange { start, end }`
  - `ReadError::InvalidStartChannel` → `ReadError::InvalidChannel { index, total }`
  - `ReadError::InvalidNumChannels` → `ReadError::InvalidChannelCount`
  - `WriteError::DecodingError` → `WriteError::Encode`
  - `ResampleError::ResamplerConstructionError` → `ResampleError::Construction`
  - `ResampleError::ResampleError` → `ResampleError::Process`
- **Breaking:** `audio-blocks` bumped from 0.6 to 0.7, which simplifies its type
  names too, e.g. `AudioBlockInterleaved` → `Interleaved`.
- Various transitive dependency updates (`rubato`, `symphonia`, etc.).

### Fixed

- Resample errors are returned as `ReadError::Resample` instead of being silently
  converted to `ReadError::NoTrack`.

## [0.3.2] - 2026-01-29

### Fixed

- Reading returned the wrong sample rate.
- The resampler got the wrong channel count when not all channels were read.
- A failed resample returns an error instead of panicking. For compatibility it
  is still reported as an existing error variant.

## [0.3.1] - 2026-01-29

### Added

- Re-export `audio-blocks`, to simplify usage.

## [0.3.0] - 2026-01-29

### Added

- Basic resampling support via `AudioReadConfig.sample_rate` (rubato-based).
- `Audio` struct (interleaved samples + sample rate + channel count).
- `audio_read_block` and `audio_write_block` helpers behind the `audio-blocks`
  feature.
- Data integrity and resampling tests for wav fixtures.

### Changed

- `audio_read` returns `Audio` instead of `AudioData`.
- `audio_write` expects interleaved samples plus an explicit channel count.
- The default features enable all codecs via `all-codecs` (including
  royalty-encumbered formats).
- `audio-blocks` is optional and gated behind the `audio-blocks` feature.
- Dependency updates (notably `audio-blocks` and `rubato`).

## [0.2.0] - 2025-10-27

### Fixed

- Channel decoding.

## [0.1.0] - 2025-10-26

### Added

- Initial release: basic audio I/O for files.

[Unreleased]: https://github.com/neodsp/audio-file/compare/0.5.1...HEAD
[0.5.1]: https://github.com/neodsp/audio-file/compare/0.5.0...0.5.1
[0.5.0]: https://github.com/neodsp/audio-file/compare/0.4.1...0.5.0
[0.4.1]: https://github.com/neodsp/audio-file/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/neodsp/audio-file/compare/0.3.2...0.4.0
[0.3.2]: https://github.com/neodsp/audio-file/compare/0.3.1...0.3.2
[0.3.1]: https://github.com/neodsp/audio-file/compare/0.3.0...0.3.1
[0.3.0]: https://github.com/neodsp/audio-file/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/neodsp/audio-file/compare/0.1.0...0.2.0
[0.1.0]: https://github.com/neodsp/audio-file/releases/tag/0.1.0
