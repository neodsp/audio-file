#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

# Repeat the one-second source to exercise ranges on both sides of the reader's
# one-second seek threshold. The Matroska files use 44.1 kHz to expose
# millisecond PTS rounding; native Ogg uses an exact 48 kHz time base to exercise
# the actual seek path.
ffmpeg -y -stream_loop 2 -i test_data/test_1ch.wav -ar 44100 -map_metadata -1 \
    -c:a flac test_data/test_flac.mka
ffmpeg -y -stream_loop 2 -i test_data/test_1ch.wav -ar 44100 -map_metadata -1 \
    -c:a libvorbis -q:a 5 test_data/test_vorbis.mka
ffmpeg -y -stream_loop 2 -i test_data/test_1ch.wav -ar 48000 -map_metadata -1 \
    -c:a libvorbis -q:a 5 test_data/test_vorbis.ogg

# 48 kHz in Matroska, where every millisecond tick is a whole number of frames.
# The seek decision is then the only thing that keeps this container off the seek
# path, so ranges past the one-second threshold cover the anchoring for an exact
# time base as well.
ffmpeg -y -stream_loop 2 -i test_data/test_1ch.wav -ar 48000 -map_metadata -1 \
    -c:a flac test_data/test_flac_48k.mka

# LAME records its encoder delay and padding in the Xing header, which symphonia
# signals as a negative PTS plus a start trim, so frame 0 of the read has to be
# the first playable frame rather than the first decoded one.
ffmpeg -y -i test_data/test_1ch.wav -map_metadata -1 \
    -c:a libmp3lame -q:a 5 test_data/test_mp3.mp3

# Deliberately make Matroska's Channels element disagree with the authoritative
# FLAC stream info. The generated element is `9f 81 02` (two channels); changing
# its payload to one leaves the encoded FLAC stereo layout untouched.
ffmpeg -y -i test_data/test_4ch.wav -t 0.02 -ac 2 -map_metadata -1 \
    -c:a flac test_data/test_declared_mono_decoded_stereo.mka
python3 - <<'PY'
from pathlib import Path

path = Path("test_data/test_declared_mono_decoded_stereo.mka")
data = path.read_bytes()
old = bytes.fromhex("e1919f8102b5")
new = bytes.fromhex("e1919f8101b5")
assert data.count(old) == 1, "unexpected Matroska audio element layout"
path.write_bytes(data.replace(old, new, 1))
PY

# Deliberately make Matroska's SamplingFrequency element disagree with the
# authoritative FLAC stream info, the same way as the Channels element above.
# The element is `b5 88` followed by a big-endian double; halving the declared
# rate leaves the encoded 44.1 kHz FLAC stream untouched.
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -ar 44100 -map_metadata -1 \
    -c:a flac test_data/test_declared_rate_mismatch.mka
python3 - <<'PY'
import struct
from pathlib import Path

path = Path("test_data/test_declared_rate_mismatch.mka")
data = path.read_bytes()
old = b"\xb5\x88" + struct.pack(">d", 44100.0)
new = b"\xb5\x88" + struct.pack(">d", 22050.0)
assert data.count(old) == 1, "unexpected Matroska sampling frequency element"
path.write_bytes(data.replace(old, new, 1))
PY

# Concatenated Ogg bitstreams are a chained stream. Each link is independently
# valid, but their decoded channel counts intentionally change from mono to stereo.
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -map_metadata -1 \
    -c:a libvorbis -q:a 3 /tmp/audio-file-mono.ogg
ffmpeg -y -i test_data/test_4ch.wav -t 0.02 -ac 2 -map_metadata -1 \
    -c:a libvorbis -q:a 3 /tmp/audio-file-stereo.ogg
cat /tmp/audio-file-mono.ogg /tmp/audio-file-stereo.ogg \
    > test_data/test_channel_count_change.ogg
rm /tmp/audio-file-mono.ogg /tmp/audio-file-stereo.ogg

# The same for a sample rate that changes between the links of the chain, which
# cannot be represented in a single buffer either.
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -ar 48000 -map_metadata -1 \
    -c:a libvorbis -q:a 3 /tmp/audio-file-48k.ogg
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -ar 44100 -map_metadata -1 \
    -c:a libvorbis -q:a 3 /tmp/audio-file-44k.ogg
cat /tmp/audio-file-48k.ogg /tmp/audio-file-44k.ogg \
    > test_data/test_sample_rate_change.ogg
rm /tmp/audio-file-48k.ogg /tmp/audio-file-44k.ogg

# Two audio tracks, the default one in a codec symphonia's Matroska demuxer
# names but has no decoder for. Track selection has to skip it and fall back to
# the decodable PCM track. AC-3 is used because it survives a container
# round-trip with full codec parameters, so the track only becomes unusable at
# decoder construction time.
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -map 0:a:0 -map 0:a:0 -map_metadata -1 \
    -c:a:0 ac3 -c:a:1 pcm_s16le -disposition:a:0 default -disposition:a:1 0 \
    test_data/test_unusable_default.mka
