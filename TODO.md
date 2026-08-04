# Release 0.5.0 To-Do

Issues found while reviewing `next` against `main`. Complete the release-blocking items before publishing 0.5.0.

## Release blockers

### [x] Fix frame-range selection for compressed/container formats

- **Priority:** High
- **Relevant code:** `src/reader.rs:326-332`, `src/reader.rs:387-431`

Decoded buffers are currently positioned directly from each packet's PTS. Packet timestamps may be coarser than one audio frame, and a decoder's output does not always correspond directly to the current packet's nominal PTS. This can produce overlapping, missing, or prematurely terminated ranges.

Observed regressions with FFmpeg-generated 44.1 kHz Matroska files:

- FLAC: reading frames `4521..4621` returned 122 frames instead of 100 because millisecond-quantized packet timestamps made adjacent decoded buffers overlap by 22 frames.
- Vorbis: reading frames `0..100` returned no frames because warm-up packets produced empty buffers and the first non-empty buffer was assigned a later packet PTS.

Acceptance criteria:

- Every valid `start..stop` request returns at most `stop - start` frames.
- A range read matches the corresponding slice of a full decode.
- Decoder warm-up after opening or seeking does not shift or discard playable frames.
- Packet timestamp quantization does not duplicate frames.
- Add regression fixtures/tests for FLAC-in-Matroska and Vorbis-in-Matroska, including ranges before and after the seek threshold.

## Reader correctness

### [x] Make default-track fallback reject null or unusable codecs

- **Priority:** Medium
- **Relevant code:** `src/reader.rs:174-185`, `src/reader.rs:259-261`, `release-notes.md:10-13`

`default_track(TrackType::Audio)` can return a default-marked track whose codec is named by the demuxer but has no registered decoder, such as AC-3, DTS or TrueHD in Matroska, or a codec excluded by the enabled features. Checking only `codec_params.is_some()` accepts that track, so no other audio track is ever tried and decoder creation fails even if a usable one exists.

The original report described this as a null codec ID passing the `codec_params.is_some()` check. That variant is not reachable with symphonia 0.6: only the Matroska demuxer sets the default-track flag, and it reports unknown codec IDs as absent codec parameters rather than as a null audio codec, which `default_track` already skips. Only the isomp4 demuxer produces `CODEC_ID_NULL_AUDIO`, and it never marks a default track. The null-codec check is therefore kept as a guard, but the reachable defect is the missing decoder-construction check.

Acceptance criteria:

- A default track is selected only when its audio codec is non-null and a decoder can be constructed.
- If the default track is unusable, reading falls back to another decodable audio track.
- Add a multi-track regression test with an unusable default track and a usable alternate track.
- Ensure `release-notes.md` describes the implemented selection behavior accurately.

### [x] Validate channel selection against the authoritative decoded layout

- **Priority:** Medium
- **Relevant code:** `src/reader.rs:227-233`, `src/reader.rs:352-384`

Channel selection is rejected up front using the container-declared channel count, even though the decoded specification is treated as authoritative later. For example, metadata declaring mono can reject `start_channel: Some(1)` even if the decoder produces stereo.

Acceptance criteria:

- When audio packets exist, validate channel selection against the first decoded layout.
- Continue using the declared layout as a fallback for files with no decoded audio frames.
- Detect and report actual mid-stream channel-count changes.
- Add tests for container metadata that disagrees with the decoded channel layout and for empty files.

### [x] Take the sample rate from the decoded audio as well

- **Priority:** High
- **Relevant code:** `src/reader.rs:228-291`, `src/reader.rs:309-320`

The channel count was made decoder-authoritative, but the sample rate was still taken from the container. Symphonia's Matroska demuxer reports the container's `SamplingFrequency` element verbatim (`symphonia-format-mkv/src/codecs.rs:69`), and only some decoders amend their codec parameters from the bitstream headers, so a declaration that contradicts the stream is passed through. Reading a 44.1 kHz FLAC-in-Matroska file whose container declares 22.05 kHz returned all 132300 frames labelled as 22050 Hz, resolved `Position::Time` against the wrong rate (a one-second stop yielded half a second of audio) and resampled from the wrong input rate.

The reader now decodes the first packet up front and uses the specification of the decoded audio for the sample rate and as the channel-count fallback. The container declaration is only used for files without a single decodable packet.

Acceptance criteria:

- The returned sample rate is the rate of the decoded audio.
- Time positions and the resampling ratio are resolved against that rate.
- A packet that decodes at a different rate than the probed one is reported as `SampleRateChanged`.
- Add a regression fixture/test whose container rate contradicts the codec's own header.

## Release automation

### [x] Align release workflow tags with the repository convention

- **Priority:** Medium
- **Relevant code:** `.github/workflows/release.yml:3-6`

Release tags remain unprefixed (`0.1.0`, `0.5.0`, etc.). The workflow now follows this established convention and compares the complete tag directly with the crate version.

Acceptance criteria:

- Decide whether releases use `0.5.0` or `v0.5.0`.
- Update the workflow trigger and version comparison accordingly.
- Document any convention change in the release process.
- Verify the workflow against both a matching tag and a mismatched tag.

### [x] Make reduced-feature CI actually disable Symphonia defaults

- **Priority:** Medium
- **Relevant code:** `Cargo.toml:19`, `.github/workflows/tests.yml:28`

`cargo test --no-default-features --features wav,pcm` disables this crate's defaults, but `symphonia = "0.6"` still enables Symphonia's default codec, container, metadata, and SIMD features. The test therefore cannot detect broken `wav` or `pcm` feature mappings.

Acceptance criteria:

- Set `default-features = false` for Symphonia.
- Ensure `all-codecs` still enables the complete intended default format/codec set.
- Ensure each documented individual feature enables only its intended Symphonia component.
- Test required feature combinations such as `wav,pcm`, `ogg,vorbis`, and `isomp4,aac`.
- Confirm `cargo tree -e features --no-default-features --features wav,pcm` does not include unrelated codecs or containers.

### [x] Close the gaps in the quality and test workflows

- **Priority:** Medium
- **Relevant code:** `.github/workflows/quality.yml`, `.github/workflows/tests.yml:29-45`, `taplo.toml`

The reduced-feature job ran a single hand-picked test by name filter, and `cargo test` exits successfully when a filter matches nothing, so renaming that test would have turned the job into a silent no-op. Clippy, the doc build and the MSRV check ran without `--all-features`, so `read_block` and `write_block` were never linted or documented under `-D warnings`, and the MSRV check ignored the tests and the lock file.

Acceptance criteria:

- Every test that needs a fixture format is gated on the features of that format, so the whole suite runs for each feature combination instead of a filtered subset.
- Clippy runs over all features and over a reduced feature set, both with `--all-targets`.
- The doc build and the MSRV check cover all features; the MSRV check also covers the tests and uses `--locked`.
- `taplo fmt --check` ignores the generated `Cargo.toml` copies under `target/`.
- Re-running the release workflow for an existing tag updates the release instead of failing.

## Test coverage

### [x] Fill the frames of a discarded packet and cover the recovery path

- **Priority:** Medium
- **Relevant code:** `src/reader.rs:511-530`, `src/reader.rs:604-624`

A packet the decoder rejects was skipped and the position recovered from the next timestamp, but the frames it would have carried were not replaced. Everything after the hole therefore moved earlier in the output, so the read silently returned audio that no longer lined up with the requested frame positions. Corrupting one FLAC frame header in the 48 kHz Matroska fixture lost exactly one packet of 4608 frames.

The hole is now filled with silence, capped by the frames the read can still produce so that a corrupt timestamp cannot request an enormous fill. A hole before the first copied frame is only filled when the read was not seeked, because after a seek the first packet may simply start later than requested and its frames were never lost.

Acceptance criteria:

- A discarded packet does not change the length of the read.
- The frames around the hole stay at their own positions.
- The hole is silent, and covers no more than the discarded packet.
- The fill is bounded when the file length is unknown.

### [x] Cover the remaining error paths and the seek decision

- **Priority:** Medium
- **Relevant code:** `src/reader.rs:585-604`, `src/reader.rs:140-144`, `src/reader.rs:556-576`

`SampleRateChanged`, `TooManyChannels` and `NoChannels` had no test, `read_block` was only covered by a doc test, and the seek decision was an inline condition that no test could reach. The `u16` channel conversion and the channel-count fallback are now small functions, `should_seek` holds the seek decision, and a chained Ogg fixture changes its sample rate mid-stream.

Note on the Matroska seek guard: it is not what makes Matroska reads correct. Measured on the new 48 kHz fixture, symphonia's accurate seek reports its landing honestly (288 ms for a 250 ms target), `seek_landing_is_safe` accepts it, and all ranges are correct even with the guard removed. The guard only avoids a seek attempt that would often be undone by reopening the file, so it is now documented as a performance choice and covered by a unit test of the decision. Dropping it would enable seeking for Matroska; that is a deliberate call to make, not an oversight.

Acceptance criteria:

- Every `ReadError` variant that a read can produce is covered by a test.
- `read_block` is covered by a real test, not only by a doc example.
- The seek decision is unit-tested, including the Matroska case.
- The encoder delay and padding claim in `release-notes.md` has a fixture: the MP3 fixture reads back with the exact length of the encoded signal, and no frame shift fits the encoded signal better than no shift.

## Documentation

### [x] Correct the selective-decoding claim

- **Priority:** Low
- **Relevant code:** `src/reader.rs:109-116`, `README.md:97`

The documentation says the crate only decodes and stores the selected range. In practice, small offsets decode from the beginning, and large offsets seek early and decode warm-up packets. Only selected frames are stored.

Acceptance criteria:

- State that only selected frames are stored.
- Explain briefly that earlier packets may be decoded for seeking and codec warm-up.
- Keep `README.md` and crate-level documentation synchronized.

## Final validation

After completing the tasks above, run:

- [x] `cargo fmt --check`
- [x] `taplo fmt --check`
- [x] `cargo clippy --locked --all-features --all-targets --workspace -- -D warnings`
- [x] `cargo clippy --locked --no-default-features --features wav,pcm --all-targets --workspace -- -D warnings`
- [x] `cargo test --locked --all-features --all-targets --workspace`
- [x] `cargo test --locked --doc --workspace`
- [x] `cargo test --locked --all-features --doc --workspace`
- [x] Reduced-feature tests with Symphonia defaults genuinely disabled, for `wav,pcm`, `ogg,vorbis`, `isomp4,aac`, `mkv,flac` and `mp3`
- [x] Compressed-format range regression tests
- [x] `cargo +1.88.0 check --locked --all-features --all-targets --workspace` (MSRV)
- [x] `cargo doc --locked --all-features --no-deps`
- [x] `cargo rdme --check`
- [x] `cargo package --locked --allow-dirty`
- [x] `git diff --check main...HEAD`

## Follow-ups, not release blocking

- `cargo package` ships this file and `utils/`. Consider removing `TODO.md` before
  tagging, or adding a `package.exclude` for the internal files.
- The recovery after a discarded packet is only frame-exact when the timestamps
  are: at 44.1 kHz in Matroska the millisecond timestamp locates the next packet
  to within about 22 frames, so a hole in such a file can stay off by that much.
  Nothing in the container can do better.
- A hole before the first copied frame is not filled after a seek, so a format
  reader that reported a safe landing but positioned itself after the requested
  start would still produce a short read. Verifying the first anchored packet
  against the requested start, and decoding again from the beginning when it is
  too late, would close that gap and make the Matroska seek guard unnecessary.
