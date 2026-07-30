# Test assets

## `speech-16k.wav`

The speech fixture used by the test suite, the latency harness and
`iris-spike --self-test`. It exists so the whole pipeline can be exercised
without a microphone.

- 5.38 s, 16 kHz, mono, 16-bit PCM (~168 kB)
- Content: *"The quick brown fox jumps over the lazy dog. Iris turns speech into
  text instantly."*

Synthesised locally with `espeak-ng`, so it carries no third-party licence:

```bash
espeak-ng -v en-us -s 178 -p 45 -w raw.wav \
  "The quick brown fox jumps over the lazy dog. Iris turns speech into text instantly."

ffmpeg -y -i raw.wav -ac 1 -ar 16000 -sample_fmt s16 \
  -af "volume=1.6,highpass=f=80,silenceremove=stop_periods=-1:stop_duration=0.35:stop_threshold=-45dB" \
  speech-16k.wav
```

It is already at the pipeline's native format, so `audio::read_wav` passes it
through without resampling. To exercise the resampler instead, point the harness
at any other WAV: `iris-harness --wav path/to/other.wav`.

Synthetic speech is a *harder* input than a real voice for a recogniser, so a
cloud engine transcribing this correctly is a meaningful signal. It is around
5 seconds because that is the utterance length the latency target is defined on.
