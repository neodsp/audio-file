<!-- cargo-rdme start -->

# audio-file

A simple library to read and write audio files on your disk.

The library can read many formats and can write only to wav files.

## Quick Start

Read a file into interleaved `f32` (or `f64`) samples, full scale at `±1.0`:

```rust
let audio = audio_file::read::<f32>("test_data/test_1ch.wav", audio_file::ReadConfig::default())?;

let samples = &audio.samples_interleaved;
let sample_rate = audio.sample_rate;
let num_channels = audio.num_channels;
```

Write interleaved samples back out as a wav file:

```rust
let samples = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]; // interleaved
let num_channels = 2;
let sample_rate = 48_000;

audio_file::write(
    "output.wav",
    &samples,
    num_channels,
    sample_rate,
    audio_file::WriteConfig::default(),
)?;
```

That covers most uses. Everything below is optional.

## Features

**With no default features, this crate reads and writes wav** through a built-in decoder, with
almost no dependencies. Everything else is additive on top of that.

| Feature | Default | What it adds |
|---------|---------|--------------|
| `all-codecs` | yes | **Read every common codec**: MP3, FLAC, AAC, ALAC, Vorbis, and the MP4, Ogg, Matroska, AIFF and CAF containers |
| `resample` | yes | **Resample while reading**, via `rubato` |
| `simd` | yes | Symphonia's SIMD optimizations. Only does something alongside a codec feature |
| `audio-blocks` | no | Read and write any channel layout, via [`audio-blocks`](https://docs.rs/audio-blocks) |

Turning a feature off removes the API it brings rather than making it fail at runtime.
Without `resample`, [`ReadConfig`](https://docs.rs/audio-file/latest/audio_file/reader/struct.ReadConfig.html) has no `sample_rate` field at all, so asking for a rate
nothing would resample to does not compile.

## Reading

[`ReadConfig`](https://docs.rs/audio-file/latest/audio_file/reader/struct.ReadConfig.html) selects what to read. Only the selected frames are stored, though the reader
may decode and discard earlier packets for accurate seeking and codec warm-up.

Positions are given as a [`Position`](https://docs.rs/audio-file/latest/audio_file/reader/enum.Position.html), in frames or in time. `start` is inclusive and `stop`
is exclusive, so frame 300 to 400 yields 100 frames. Frame 0 is the first playable frame:
encoder delay and padding, as used by formats like MP3, are not part of the timeline.

```rust
// the first half second
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        stop: audio_file::Position::Time(Duration::from_secs_f32(0.5)),
        ..Default::default()
    },
)?;

// frame 300 up to frame 400
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        start: audio_file::Position::Frame(300),
        stop: audio_file::Position::Frame(400),
        ..Default::default()
    },
)?;
```

Channels are selected the same way, with `start_channel` and `num_channels`:

```rust
// the first two channels
let audio = audio_file::read::<f32>(
    "test_data/test_4ch.wav",
    audio_file::ReadConfig {
        num_channels: Some(2),
        ..Default::default()
    },
)?;

// channel 2 and 3, skipping the first
let audio = audio_file::read::<f32>(
    "test_data/test_4ch.wav",
    audio_file::ReadConfig {
        start_channel: Some(1),
        num_channels: Some(2),
        ..Default::default()
    },
)?;
```

With the `resample` feature, a `sample_rate` makes the reader hand back audio at that rate
whatever the file holds:

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        sample_rate: Some(22_050),
        ..Default::default()
    },
)?;
```

A file is either read in full or not at all: a packet the decoder rejects fails the whole read
rather than being skipped over or filled in.

## Writing

Output is always wav. [`WriteConfig`](https://docs.rs/audio-file/latest/audio_file/writer/struct.WriteConfig.html) picks the sample format:

| [`SampleFormat`](https://docs.rs/audio-file/latest/audio_file/writer/enum.SampleFormat.html) | Description |
|------------------|-------------|
| `Int8` | 8-bit integer |
| `Int16` | 16-bit integer (default, for the broadest compatibility) |
| `Int32` | 32-bit integer |
| `Float32` | 32-bit float |

```rust
audio_file::write(
    "output_float32.wav",
    &samples_interleaved,
    num_channels,
    sample_rate,
    audio_file::WriteConfig {
        sample_format: audio_file::SampleFormat::Float32,
    },
)?;
```

## Other channel layouts

Interleaved is the only layout `read` and `write` speak. With the `audio-blocks` feature you
get `read_block` and `write_block`, which work in `AudioBlock`s and so handle any layout, plus
channel-wise access to what was read:

```rust
let (block, sample_rate) =
    audio_file::read_block::<f32>("test_data/test_4ch.wav", ReadConfig::default())?;
let left: Vec<f32> = block.channel_iter(0).copied().collect();

let block = Sequential::from_slice(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0], 2);
audio_file::write_block("output_layout.wav", block, 48_000, WriteConfig::default())?;
```

## Picking individual codecs

`all-codecs` is a convenience for "read anything". To keep the dependency tree small, name
only the formats you need instead:

| Format | Feature flags |
|--------|---------------|
| WAV, integer PCM and IEEE float | none, built in |
| WAV, A-law, mu-law and ADPCM | `wav-compressed` |
| FLAC | `flac` |
| MP1, MP2, MP3 | `mp1`, `mp2`, `mp3` |
| AAC (raw ADTS) | `aac` |
| MP4, M4A | `isomp4` plus `aac` or `alac` |
| Ogg | `ogg` plus `vorbis` or `flac` |
| Matroska (MKA, MKV) | `mkv` plus the codec inside: `pcm`, `flac`, `vorbis`, `aac`, `alac` |
| AIFF | `aiff` plus `pcm` |
| CAF | `caf` plus `pcm` or `alac` |

A container and the codec inside it are separate flags, so a format that can hold several
codecs needs one of each. Enabling only the container reads no file at all. A file no decoder
in the build can read fails with [`ReadError::UnsupportedFormat`](https://docs.rs/audio-file/latest/audio_file/reader/enum.ReadError.html#variant.UnsupportedFormat).

Some formats have channel ceilings below what the format itself allows, see the
[`reader`](https://docs.rs/audio-file/latest/audio_file/reader/) module docs.

<!-- cargo-rdme end -->
