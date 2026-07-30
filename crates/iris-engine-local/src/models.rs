//! Lazy model download from Hugging Face with size checks and progress hooks.
//!
//! Models are never bundled in the installer. On first use of a given engine
//! the relevant files are fetched into a configurable directory
//! (`IRIS_MODEL_DIR`, or the path passed to [`ensure_model`]).
//!
//! # Disk cost (approximate, one-time download)
//!
//! | Model set                         | Disk   |
//! |-----------------------------------|--------|
//! | Zipformer streaming int8          | ~71 MB |
//! | whisper base.en q5_1              | ~60 MB |
//! | Silero VAD (ggml)                 | ~0.9 MB|
//! | Parakeet-TDT 0.6B q8_0 (follow-up)| 638 MB |
//!
//! Default recommended set today (Zipformer + base.en + VAD): **~132 MB**.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Env var: override the default model cache directory.
pub const DEFAULT_MODEL_DIR_ENV: &str = "IRIS_MODEL_DIR";

/// Progress callback: `(bytes_downloaded, total_bytes_if_known)`.
pub type ProgressFn = dyn Fn(u64, Option<u64>) + Send + Sync;

/// Stable identifier for a catalogued model artefact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelId {
    /// Streaming Zipformer int8 encoder (chunk-16 left-64).
    ZipformerEncoder,
    /// Streaming Zipformer decoder (fp32; small).
    ZipformerDecoder,
    /// Streaming Zipformer int8 joiner.
    ZipformerJoiner,
    /// Zipformer tokens.txt.
    ZipformerTokens,
    /// whisper.cpp base.en q5_1.
    WhisperBaseEnQ5_1,
    /// Silero VAD ggml (whisper.cpp-compatible).
    SileroVad,
    /// Future: Parakeet-TDT 0.6B q8_0 via whisper.cpp GGML (no Rust bind yet).
    #[allow(dead_code)]
    ParakeetTdt06bQ8_0,
}

impl ModelId {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelId::ZipformerEncoder => "zipformer-encoder-int8",
            ModelId::ZipformerDecoder => "zipformer-decoder",
            ModelId::ZipformerJoiner => "zipformer-joiner-int8",
            ModelId::ZipformerTokens => "zipformer-tokens",
            ModelId::WhisperBaseEnQ5_1 => "whisper-base.en-q5_1",
            ModelId::SileroVad => "silero-vad",
            ModelId::ParakeetTdt06bQ8_0 => "parakeet-tdt-0.6b-q8_0",
        }
    }
}

/// One downloadable model file.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: ModelId,
    /// Relative path under the model dir.
    pub relative_path: &'static str,
    /// Hugging Face resolve URL.
    pub url: &'static str,
    /// Expected size in bytes (from HF Content-Length). Used as a basic integrity check.
    pub expected_bytes: u64,
    /// Optional SHA-256 hex digest. When `None`, only size is verified.
    pub sha256_hex: Option<&'static str>,
    /// Human-readable disk cost note.
    pub disk_note: &'static str,
}

/// Built-in catalogue of models Iris local engines know how to fetch.
pub struct ModelCatalog;

impl ModelCatalog {
    pub fn all() -> &'static [ModelSpec] {
        static CATALOG: OnceLock<Vec<ModelSpec>> = OnceLock::new();
        CATALOG.get_or_init(|| {
            vec![
                ModelSpec {
                    id: ModelId::ZipformerEncoder,
                    relative_path: "zipformer-en-2023-06-26/encoder-epoch-99-avg-1-chunk-16-left-64.int8.onnx",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/main/encoder-epoch-99-avg-1-chunk-16-left-64.int8.onnx",
                    expected_bytes: 71_082_637,
                    sha256_hex: None,
                    disk_note: "~68 MB (Zipformer int8 encoder)",
                },
                ModelSpec {
                    id: ModelId::ZipformerDecoder,
                    relative_path: "zipformer-en-2023-06-26/decoder-epoch-99-avg-1-chunk-16-left-64.onnx",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/main/decoder-epoch-99-avg-1-chunk-16-left-64.onnx",
                    expected_bytes: 2_092_621,
                    sha256_hex: None,
                    disk_note: "~2 MB (Zipformer decoder)",
                },
                ModelSpec {
                    id: ModelId::ZipformerJoiner,
                    relative_path: "zipformer-en-2023-06-26/joiner-epoch-99-avg-1-chunk-16-left-64.int8.onnx",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/main/joiner-epoch-99-avg-1-chunk-16-left-64.int8.onnx",
                    expected_bytes: 259_335,
                    sha256_hex: None,
                    disk_note: "~0.25 MB (Zipformer int8 joiner)",
                },
                ModelSpec {
                    id: ModelId::ZipformerTokens,
                    relative_path: "zipformer-en-2023-06-26/tokens.txt",
                    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/main/tokens.txt",
                    expected_bytes: 5_048,
                    sha256_hex: None,
                    disk_note: "~5 KB (tokens)",
                },
                ModelSpec {
                    id: ModelId::WhisperBaseEnQ5_1,
                    relative_path: "whisper/ggml-base.en-q5_1.bin",
                    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en-q5_1.bin",
                    expected_bytes: 59_721_011,
                    sha256_hex: None,
                    disk_note: "~60 MB (whisper base.en q5_1)",
                },
                ModelSpec {
                    id: ModelId::SileroVad,
                    relative_path: "whisper/ggml-silero-v5.1.2.bin",
                    url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
                    expected_bytes: 885_098,
                    sha256_hex: None,
                    disk_note: "~0.9 MB (Silero VAD ggml)",
                },
                ModelSpec {
                    id: ModelId::ParakeetTdt06bQ8_0,
                    relative_path: "parakeet/ggml-parakeet-tdt-0.6b-v3-q8_0.bin",
                    url: "https://huggingface.co/ggml-org/parakeet-GGUF/resolve/main/ggml-parakeet-tdt-0.6b-v3-q8_0.bin",
                    expected_bytes: 638_000_000, // approximate; exact size may vary by release
                    sha256_hex: None,
                    disk_note: "~638 MB (Parakeet-TDT 0.6B q8_0 — fast-follow, no Rust bind yet)",
                },
            ]
        })
    }

    pub fn get(id: ModelId) -> &'static ModelSpec {
        Self::all()
            .iter()
            .find(|s| s.id == id)
            .expect("catalog incomplete")
    }

    /// Files needed for the streaming Zipformer engine (~71 MB total).
    pub fn zipformer_set() -> &'static [ModelId] {
        &[
            ModelId::ZipformerEncoder,
            ModelId::ZipformerDecoder,
            ModelId::ZipformerJoiner,
            ModelId::ZipformerTokens,
        ]
    }

    /// Files needed for the whisper finalizer + VAD (~61 MB total).
    pub fn whisper_set() -> &'static [ModelId] {
        &[ModelId::WhisperBaseEnQ5_1, ModelId::SileroVad]
    }
}

/// Resolve the model directory: explicit path, else `$IRIS_MODEL_DIR`, else
/// `$HOME/.cache/iris/models` (or `./.iris-models` if HOME is unset).
pub fn default_model_dir() -> PathBuf {
    if let Ok(p) = std::env::var(DEFAULT_MODEL_DIR_ENV) {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache/iris/models");
    }
    PathBuf::from(".iris-models")
}

/// Ensure a single catalogued model is present under `model_dir`, downloading
/// if needed. Returns the absolute path to the file.
///
/// Safe to call concurrently for the same model: losers of a download race
/// re-check the destination after a failed rename and succeed if another
/// thread finished first.
pub fn ensure_model(
    id: ModelId,
    model_dir: &Path,
    progress: Option<&ProgressFn>,
) -> Result<PathBuf> {
    let spec = ModelCatalog::get(id);
    let dest = model_dir.join(spec.relative_path);
    if dest.is_file() {
        verify_existing(&dest, spec)?;
        return Ok(dest);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating model dir {}", parent.display()))?;
    }
    match download_to(&dest, spec, progress) {
        Ok(()) => {}
        Err(e) => {
            // Another concurrent ensure_model may have won the race.
            if dest.is_file() {
                verify_existing(&dest, spec)?;
                return Ok(dest);
            }
            return Err(e);
        }
    }
    verify_existing(&dest, spec)?;
    Ok(dest)
}

/// Ensure every id in `ids` is present. Returns paths in the same order.
pub fn ensure_models(
    ids: &[ModelId],
    model_dir: &Path,
    progress: Option<&ProgressFn>,
) -> Result<Vec<PathBuf>> {
    ids.iter()
        .map(|&id| ensure_model(id, model_dir, progress))
        .collect()
}

fn verify_existing(path: &Path, spec: &ModelSpec) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    let len = meta.len();
    // Allow ±1% size drift for LFS/CDN variance, but reject obvious corruption.
    let min = spec.expected_bytes.saturating_mul(99) / 100;
    let max = spec.expected_bytes.saturating_mul(101) / 100 + 1024;
    // Parakeet size is approximate; skip tight size check for that entry.
    if !matches!(spec.id, ModelId::ParakeetTdt06bQ8_0) && (len < min || len > max) {
        bail!(
            "model {} at {} has size {len}, expected ~{} bytes — delete and re-download",
            spec.id.as_str(),
            path.display(),
            spec.expected_bytes
        );
    }
    if let Some(expected) = spec.sha256_hex {
        let actual = sha256_file(path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!(
                "model {} SHA-256 mismatch at {}: got {actual}, expected {expected}",
                spec.id.as_str(),
                path.display()
            );
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn download_to(dest: &Path, spec: &ModelSpec, progress: Option<&ProgressFn>) -> Result<()> {
    // Unique temp name so concurrent ensure_model calls do not clobber each other.
    // `Path::with_extension` only replaces the *last* suffix, which is wrong for
    // names like `foo.int8.onnx` — append a process/random suffix instead.
    let tmp = dest.with_file_name(format!(
        "{}.part.{}-{:x}",
        dest.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    tracing::info!(
        model = spec.id.as_str(),
        url = spec.url,
        dest = %dest.display(),
        "downloading model"
    );

    let resp = ureq::get(spec.url)
        .set("User-Agent", "iris-engine-local/0.1")
        .call()
        .with_context(|| format!("GET {}", spec.url))?;

    if !(200..300).contains(&resp.status()) {
        bail!("download {} returned HTTP {}", spec.url, resp.status());
    }

    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .or(Some(spec.expected_bytes));

    let mut reader = resp.into_reader();
    let mut file = File::create(&tmp)
        .with_context(|| format!("creating temp {}", tmp.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let n = reader.read(&mut buf).context("reading download body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        if let Some(cb) = progress {
            cb(downloaded, total);
        }
    }
    file.flush()?;
    drop(file);

    // If another thread already placed the final file, drop our temp and win.
    if dest.is_file() {
        let _ = fs::remove_file(&tmp);
        return Ok(());
    }

    fs::rename(&tmp, dest).with_context(|| {
        format!("renaming {} → {}", tmp.display(), dest.display())
    })?;
    Ok(())
}

/// Paths to the four Zipformer files after ensure, for constructing a recognizer.
#[derive(Debug, Clone)]
pub struct ZipformerPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

impl ZipformerPaths {
    pub fn ensure(model_dir: &Path, progress: Option<&ProgressFn>) -> Result<Self> {
        let paths = ensure_models(ModelCatalog::zipformer_set(), model_dir, progress)?;
        Ok(Self {
            encoder: paths[0].clone(),
            decoder: paths[1].clone(),
            joiner: paths[2].clone(),
            tokens: paths[3].clone(),
        })
    }
}

/// Paths to whisper model + Silero VAD.
#[derive(Debug, Clone)]
pub struct WhisperPaths {
    pub model: PathBuf,
    pub vad: PathBuf,
}

impl WhisperPaths {
    pub fn ensure(model_dir: &Path, progress: Option<&ProgressFn>) -> Result<Self> {
        let paths = ensure_models(ModelCatalog::whisper_set(), model_dir, progress)?;
        Ok(Self {
            model: paths[0].clone(),
            vad: paths[1].clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_disk_notes_for_all_models() {
        for spec in ModelCatalog::all() {
            assert!(!spec.disk_note.is_empty(), "{:?}", spec.id);
            assert!(!spec.url.is_empty());
            assert!(!spec.relative_path.is_empty());
            assert!(spec.expected_bytes > 0);
        }
    }

    #[test]
    fn zipformer_set_is_four_files() {
        assert_eq!(ModelCatalog::zipformer_set().len(), 4);
    }

    #[test]
    fn default_model_dir_respects_env() {
        // Don't clobber a real IRIS_MODEL_DIR if set; just check the function
        // returns a non-empty path.
        let dir = default_model_dir();
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn ensure_model_rejects_wrong_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ModelCatalog::get(ModelId::ZipformerTokens);
        let dest = dir.path().join(spec.relative_path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"too-small").unwrap();
        let err = ensure_model(ModelId::ZipformerTokens, dir.path(), None).unwrap_err();
        assert!(
            err.to_string().contains("size") || err.to_string().contains("expected"),
            "{err}"
        );
    }
}
