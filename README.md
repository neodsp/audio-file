<!-- cargo-rdme start -->

# audio-file

A simple library to read and write audio files on your disk.

The library can read many formats and can write only to wav files.

## Quick Start

### Read Audio

You can read most common audio formats. The default feature set enables all available codecs.

```rust
let audio = audio_file::read::<f32>("test_data/test_1ch.wav", audio_file::ReadConfig::default())?;
let sample_rate = audio.sample_rate;
let num_channels = audio.num_channels;
let samples = &audio.samples_interleaved;
```

With `audio-blocks`, you can read straight into an `AudioBlock`, which adds simple channel-based read helpers:

```rust
let (block, sample_rate) = audio_file::read_block::<f32>("test_data/test_1ch.wav", audio_file::ReadConfig::default())?;
```

### Write Audio

You can only write wav files. The `audio_file::write` function expects interleaved samples.

```rust
let samples = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]; // interleaved
let num_channels = 2;
let sample_rate = 48000;
audio_file::write(
    "tmp.wav",
    &samples,
    num_channels,
    sample_rate,
    audio_file::WriteConfig::default(),
)?;
```

With the `audio-blocks` feature you can write any audio layout, e.g.:

```rust
let sample_rate = 48000;

let block = AudioBlockInterleavedView::from_slice(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0], 2);
audio_file::write_block("tmp.wav", block, sample_rate, audio_file::WriteConfig::default())?;

let block = AudioBlockSequentialView::from_slice(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0], 2);
audio_file::write_block("tmp.wav", block, sample_rate, audio_file::WriteConfig::default())?;
```

## Supported Input Codecs

Default features enable all codecs (including royalty-encumbered formats) via `all-codecs`.
To opt out, disable default features and enable only what you need.

| Format | Feature Flag |
|--------|--------------|
| AAC | `aac` |
| ADPCM | `adpcm` |
| ALAC | `alac` |
| FLAC | `flac` |
| CAF | `caf` |
| ISO MP4 | `isomp4` |
| Matroska (MKV) | `mkv` |
| MP1 | `mp1` |
| MP2 | `mp2` |
| MP3 | `mp3` |
| Ogg | `ogg` |
| PCM | `pcm` |
| AIFF | `aiff` |
| Vorbis | `vorbis` |
| WAV | `wav` |

Feature flags:

- `all-codecs` enables all Symphonia codecs (this is the default).
- `audio-blocks` enables `read_block` and `write_block`.
- Individual codec flags (above) enable specific formats.


## Read and Write Options

### Reading

When reading a file you can specify the following things:

- Start and stop in frames or time
- Start channel and number of channels
- Optional resampling

The crate will try to decode and store only the parts that you selected.

### Writing

For writing audio you can select from the following sample formats:

| Format | Description |
|--------|-------------|
| `Int8` | 8-bit integer |
| `Int16` | 16-bit integer (default) |
| `Int32` | 32-bit integer |
| `Float32` | 32-bit float |

`Int16` is the default, for broader compatibility.

### Some example configs:

- resample to 22.05 kHz while reading

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        sample_rate: Some(22_050),
        ..Default::default()
    },
)?;
```

- read the first 0.5 seconds

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        stop: audio_file::Position::Time(Duration::from_secs_f32(0.5)),
        ..Default::default()
    },
)?;
```

- read from frame 300 to 400

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_1ch.wav",
    audio_file::ReadConfig {
        start: audio_file::Position::Frame(300),
        stop: audio_file::Position::Frame(400),
        ..Default::default()
    },
)?;
```

- read only the first two channels

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_4ch.wav",
    audio_file::ReadConfig {
        num_channels: Some(2),
        ..Default::default()
    },
)?;
```

- skip the first channel, reading channel 2 and 3

```rust
let audio = audio_file::read::<f32>(
    "test_data/test_4ch.wav",
    audio_file::ReadConfig {
        start_channel: Some(1),
        num_channels: Some(2),
        ..Default::default()
    },
)?;
```

- write audio samples in `Float32`

```rust
audio_file::write(
    "tmp.wav",
    &samples_interleaved,
    num_channels,
    sample_rate,
    audio_file::WriteConfig {
        sample_format: audio_file::SampleFormat::Float32,
    },
)?;
```

<!-- cargo-rdme end -->
