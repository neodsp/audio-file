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
