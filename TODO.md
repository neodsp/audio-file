# Release 0.5.0 To-Do

Issues found while reviewing `next` against `main`. Complete the release-blocking items before publishing 0.5.0.

## Release blockers

### [x] Fix frame-range selection for compressed/container formats

**Priority:** High  
**Relevant code:** `src/reader.rs:326-332`, `src/reader.rs:387-431`

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

**Priority:** Medium  
**Relevant code:** `src/reader.rs:174-185`, `src/reader.rs:259-261`, `release-notes.md:10-13`

`default_track(TrackType::Audio)` can return a default-marked track whose codec is named by the demuxer but has no registered decoder, such as AC-3, DTS or TrueHD in Matroska, or a codec excluded by the enabled features. Checking only `codec_params.is_some()` accepts that track, so no other audio track is ever tried and decoder creation fails even if a usable one exists.

The original report described this as a null codec ID passing the `codec_params.is_some()` check. That variant is not reachable with symphonia 0.6: only the Matroska demuxer sets the default-track flag, and it reports unknown codec IDs as absent codec parameters rather than as a null audio codec, which `default_track` already skips. Only the isomp4 demuxer produces `CODEC_ID_NULL_AUDIO`, and it never marks a default track. The null-codec check is therefore kept as a guard, but the reachable defect is the missing decoder-construction check.

Acceptance criteria:

- A default track is selected only when its audio codec is non-null and a decoder can be constructed.
- If the default track is unusable, reading falls back to another decodable audio track.
- Add a multi-track regression test with an unusable default track and a usable alternate track.
- Ensure `release-notes.md` describes the implemented selection behavior accurately.

### [x] Validate channel selection against the authoritative decoded layout

**Priority:** Medium  
**Relevant code:** `src/reader.rs:227-233`, `src/reader.rs:352-384`

Channel selection is rejected up front using the container-declared channel count, even though the decoded specification is treated as authoritative later. For example, metadata declaring mono can reject `start_channel: Some(1)` even if the decoder produces stereo.

Acceptance criteria:

- When audio packets exist, validate channel selection against the first decoded layout.
- Continue using the declared layout as a fallback for files with no decoded audio frames.
- Detect and report actual mid-stream channel-count changes.
- Add tests for container metadata that disagrees with the decoded channel layout and for empty files.

## Release automation

### [x] Align release workflow tags with the repository convention

**Priority:** Medium  
**Relevant code:** `.github/workflows/release.yml:3-6`

Release tags remain unprefixed (`0.1.0`, `0.5.0`, etc.). The workflow now follows this established convention and compares the complete tag directly with the crate version.

Acceptance criteria:

- Decide whether releases use `0.5.0` or `v0.5.0`.
- Update the workflow trigger and version comparison accordingly.
- Document any convention change in the release process.
- Verify the workflow against both a matching tag and a mismatched tag.

### [ ] Make reduced-feature CI actually disable Symphonia defaults

**Priority:** Medium  
**Relevant code:** `Cargo.toml:19`, `.github/workflows/tests.yml:28`

`cargo test --no-default-features --features wav,pcm` disables this crate's defaults, but `symphonia = "0.6"` still enables Symphonia's default codec, container, metadata, and SIMD features. The test therefore cannot detect broken `wav` or `pcm` feature mappings.

Acceptance criteria:

- Set `default-features = false` for Symphonia.
- Ensure `all-codecs` still enables the complete intended default format/codec set.
- Ensure each documented individual feature enables only its intended Symphonia component.
- Test required feature combinations such as `wav,pcm`, `ogg,vorbis`, and `isomp4,aac`.
- Confirm `cargo tree -e features --no-default-features --features wav,pcm` does not include unrelated codecs or containers.

## Documentation

### [ ] Correct the selective-decoding claim

**Priority:** Low  
**Relevant code:** `src/reader.rs:109-116`, `README.md:97`

The documentation says the crate only decodes and stores the selected range. In practice, small offsets decode from the beginning, and large offsets seek early and decode warm-up packets. Only selected frames are stored.

Acceptance criteria:

- State that only selected frames are stored.
- Explain briefly that earlier packets may be decoded for seeking and codec warm-up.
- Keep `README.md` and crate-level documentation synchronized.

## Final validation

After completing the tasks above, run:

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --all-features --all-targets --workspace -- -D warnings`
- [ ] `cargo test --locked --all-features --all-targets --workspace`
- [ ] `cargo test --locked --doc --workspace`
- [ ] `cargo test --locked --all-features --doc --workspace`
- [ ] Reduced-feature tests with Symphonia defaults genuinely disabled
- [ ] Compressed-format range regression tests
- [ ] `cargo doc --locked --all-features --no-deps`
- [ ] `cargo package --locked --allow-dirty`
- [ ] `git diff --check main...HEAD`
