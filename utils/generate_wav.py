#!/usr/bin/env -S uv run
# pyright: reportMissingImports=false
# /// script
# requires-python = ">=3.9"
# dependencies = [
#     "pyfar",
# ]
# ///

from pathlib import Path

import pyfar as pf

DURATION_SECONDS = 1
SAMPLE_RATE = 48_000
FREQUENCIES = [440, 554.37, 659.25, 880]  # A4, C#5, E5, A5

repository_root = Path(__file__).resolve().parent.parent
test_data = repository_root / "test_data"
n_samples = DURATION_SECONDS * SAMPLE_RATE

signal = pf.signals.sine(FREQUENCIES, n_samples, sampling_rate=SAMPLE_RATE)
fixtures = {
    test_data / "test_1ch.wav": signal[0],
    test_data / "test_4ch.wav": signal,
}

for path, fixture in fixtures.items():
    pf.io.write_audio(fixture, path, subtype="PCM_16")
    print(
        f"Generated {path.relative_to(repository_root)}: "
        f"{fixture.cshape[0]} channel(s), {fixture.n_samples} frames, "
        f"{fixture.sampling_rate} Hz"
    )

print(f"Frequencies: {FREQUENCIES} Hz")
