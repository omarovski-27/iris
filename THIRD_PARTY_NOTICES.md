# Third-party notices

Iris source code is licensed under the MIT License in [`LICENSE`](LICENSE).

This repository also includes or can optionally use third-party components with
their own licenses:

| Component | Where | License | Notes |
| --- | --- | --- | --- |
| Cascadia Mono | `crates/iris-overlay/assets/fonts/CascadiaMono-Regular.ttf` | SIL Open Font License 1.1 | The full license text is in `crates/iris-overlay/assets/fonts/OFL.txt`. |
| sherpa-onnx | optional `iris-engine-local` `streaming` feature | Apache-2.0 | Optional dependency; not part of the default shipped Windows build. |
| whisper-rs | optional `iris-engine-local` `whisper` feature | Unlicense | Optional dependency; not part of the default shipped Windows build. |

Rust crate dependencies are not vendored into this repository. Their licenses
apply when they are built or distributed as part of a binary.
