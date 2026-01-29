# audio-io

A simple library to read and write audio files on your disk.

The library can read many formats and can write only to wav files.

## Quick Start

### Read Audio

You can read most common audio formats. The default feature set enables all available codecs.

```rust
use audio_io::*;

let audio = audio_read::<f32>("test.wav", AudioReadConfig::default())?;
let sample_rate = audio.sample_rate;
let num_channels = audio.num_channels;
let samples = &audio.samples_interleaved;
```

With `audio-blocks`, you can read straight into an `AudioBlock`, which adds simple channel-based read helpers:

```rust
use audio_io::*;

let (block, sample_rate) = audio_read_block::<f32>("test.wav", AudioReadConfig::default())?;
```

### Write Audio

You can only write wav files. The `audio_write` function expects interleaved samples.

```rust
use audio_io::*;

let sample_rate = 48000;
let num_channels = 2;
let samples = [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]; // interleaved

audio_write(
    "tmp.wav",
    &samples,
    num_channels,
    sample_rate,
    AudioWriteConfig::default(),
)?;
```

With the `audio-blocks` feature you can write any audio layout, e.g.:

```rust
use audio_blocks::{AudioBlockInterleavedView, AudioBlockSequentialView};
use audio_io::*;

let sample_rate = 48000;

let block = AudioBlockInterleavedView::from_slice(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0], 2);
audio_write_block("tmp.wav", block, sample_rate, AudioWriteConfig::default())?;

let block = AudioBlockSequentialView::from_slice(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0], 2, 3);
audio_write_block("tmp.wav", block, sample_rate, AudioWriteConfig::default())?;
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
- `audio-blocks` enables `audio_read_block` and `audio_write_block`.
- Individual codec flags (above) enable specific formats.


## Read and Write Options

### Reading

When reading a file you can specify the following things:

- Start and stop in frames or time
- Start channel and number of channels
- Optional resampling

The crate will try to decode and store only the parts that you selected.

### Writing

For writing audio you can only select to store the audio in `Int16` or `Float32`.
By default `Int16` is selected, for broader compatibility.

### Some example configs:

- resample to 22.05 kHz while reading

```rust
let audio = audio_read::<f32>(
    "test.wav",
    AudioReadConfig {
        sample_rate: Some(22_050),
        ..Default::default()
    },
)?;
```

- read the first 0.5 seconds

```rust
use std::time::Duration;

let audio = audio_read::<f32>(
    "test.wav",
    AudioReadConfig {
        stop: Position::Time(Duration::from_secs_f32(0.5)),
        ..Default::default()
    },
)?;
```

- read from frame 300 to 400

```rust
let audio = audio_read::<f32>(
    "test.wav",
    AudioReadConfig {
        start: Position::Frame(300),
        stop: Position::Frame(400),
        ..Default::default()
    },
)?;
```

- read only the first two channels

```rust
let audio = audio_read::<f32>(
    "test.wav",
    AudioReadConfig {
        num_channels: Some(2),
        ..Default::default()
    },
)?;
```

- skip the first channel, reading channel 2 and 3

```rust
let audio = audio_read::<f32>(
    "test.wav",
    AudioReadConfig {
        start_channel: Some(1),
        num_channels: Some(2),
        ..Default::default()
    },
)?;
```

- write audio samples in `Float32`

```rust
audio_write(
    "tmp.wav",
    &samples_interleaved,
    num_channels,
    sample_rate,
    AudioWriteConfig {
        sample_format: WriteSampleFormat::Float32,
    },
)?;
```
