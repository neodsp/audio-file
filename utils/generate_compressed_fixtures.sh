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

# Two audio tracks, the default one in a codec symphonia's Matroska demuxer
# names but has no decoder for. Track selection has to skip it and fall back to
# the decodable PCM track. AC-3 is used because it survives a container
# round-trip with full codec parameters, so the track only becomes unusable at
# decoder construction time.
ffmpeg -y -i test_data/test_1ch.wav -t 0.02 -map 0:a:0 -map 0:a:0 -map_metadata -1 \
    -c:a:0 ac3 -c:a:1 pcm_s16le -disposition:a:0 default -disposition:a:1 0 \
    test_data/test_unusable_default.mka
