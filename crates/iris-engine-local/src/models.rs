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

/// Resolve the model directory: explicit path, else `$IRIS_MODEL_DIR`, else a
/// stable per-user cache, else `./.iris-models`.
///
/// Resolution order after the env override:
/// - **Windows:** `$LOCALAPPDATA/Iris/models`, then
///   `$USERPROFILE/AppData/Local/Iris/models`, then `$HOME/.cache/iris/models`
/// - **Unix:** `$HOME/.cache/iris/models` only (Windows env vars are ignored so
///   WSL/inherited `LOCALAPPDATA`/`USERPROFILE` cannot redirect the cache)
/// - Last resort on every OS: `./.iris-models` (cwd-relative)
pub fn default_model_dir() -> PathBuf {
    #[cfg(windows)]
    {
        resolve_model_dir(
            std::env::var(DEFAULT_MODEL_DIR_ENV).ok().as_deref(),
            std::env::var("LOCALAPPDATA").ok().as_deref(),
            std::env::var("USERPROFILE").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }
    #[cfg(not(windows))]
    {
        resolve_model_dir(
            std::env::var(DEFAULT_MODEL_DIR_ENV).ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|p| !p.is_empty())
}

#[cfg(windows)]
fn resolve_model_dir(
    iris_model_dir: Option<&str>,
    localappdata: Option<&str>,
    userprofile: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    if let Some(p) = non_empty(iris_model_dir) {
        return PathBuf::from(p);
    }
    if let Some(local) = non_empty(localappdata) {
        return PathBuf::from(local).join("Iris").join("models");
    }
    if let Some(profile) = non_empty(userprofile) {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Local")
            .join("Iris")
            .join("models");
    }
    if let Some(home) = non_empty(home) {
        return PathBuf::from(home).join(".cache").join("iris").join("models");
    }
    PathBuf::from(".iris-models")
}

#[cfg(not(windows))]
fn resolve_model_dir(iris_model_dir: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(p) = non_empty(iris_model_dir) {
        return PathBuf::from(p);
    }
    if let Some(home) = non_empty(home) {
        return PathBuf::from(home).join(".cache").join("iris").join("models");
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
        match verify_existing(&dest, spec) {
            Ok(()) => return Ok(dest),
            Err(err) => {
                // Truncated/corrupt cache must not permanently block ensure_model.
                tracing::warn!(
                    model = spec.id.as_str(),
                    path = %dest.display(),
                    error = %err,
                    "removing corrupt model cache entry for re-download"
                );
                fs::remove_file(&dest).with_context(|| {
                    format!(
                        "removing corrupt model {} at {} (verify failed: {err:#})",
                        spec.id.as_str(),
                        dest.display()
                    )
                })?;
            }
        }
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
                match verify_existing(&dest, spec) {
                    Ok(()) => return Ok(dest),
                    Err(verify_err) => {
                        let _ = fs::remove_file(&dest);
                        return Err(e).context(verify_err);
                    }
                }
            }
            return Err(e);
        }
    }
    match verify_existing(&dest, spec) {
        Ok(()) => Ok(dest),
        Err(err) => {
            let _ = fs::remove_file(&dest);
            Err(err).context("downloaded model failed verification; removed bad file")
        }
    }
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

/// Owns the download temp path and open handle; removes the path on drop unless
/// [`TempFileGuard::persist`] was called. Drops the file handle before unlink.
struct TempFileGuard {
    path: PathBuf,
    file: Option<File>,
    persist: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf, file: File) -> Self {
        Self {
            path,
            file: Some(file),
            persist: false,
        }
    }

    fn file_mut(&mut self) -> Result<&mut File> {
        self.file
            .as_mut()
            .context("download temp file already closed")
    }

    fn close_file(&mut self) {
        self.file.take();
    }

    fn persist(mut self) {
        self.file.take();
        self.persist = true;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        self.file.take();
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
    }
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
    let file = File::create(&tmp)
        .with_context(|| format!("creating temp {}", tmp.display()))?;
    let mut tmp_guard = TempFileGuard::new(tmp.clone(), file);
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let n = reader.read(&mut buf).context("reading download body")?;
        if n == 0 {
            break;
        }
        tmp_guard.file_mut()?.write_all(&buf[..n])?;
        downloaded += n as u64;
        if let Some(cb) = progress {
            cb(downloaded, total);
        }
    }
    tmp_guard.file_mut()?.flush()?;
    tmp_guard.close_file();

    // If another thread already placed the final file, drop our temp and win.
    if dest.is_file() {
        // Guard drop removes the temp.
        return Ok(());
    }

    fs::rename(&tmp, dest).with_context(|| {
        format!("renaming {} → {}", tmp.display(), dest.display())
    })?;
    tmp_guard.persist();
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

    #[cfg(windows)]
    #[test]
    fn resolve_model_dir_prefers_windows_paths_before_home_and_cwd() {
        assert_eq!(
            resolve_model_dir(
                Some("/custom"),
                Some("C:/Local"),
                Some("C:/Users/x"),
                Some("/home/x"),
            ),
            PathBuf::from("/custom")
        );
        assert_eq!(
            resolve_model_dir(None, Some("C:/Local"), Some("C:/Users/x"), Some("/home/x")),
            PathBuf::from("C:/Local").join("Iris").join("models")
        );
        assert_eq!(
            resolve_model_dir(None, None, Some("C:/Users/x"), Some("/home/x")),
            PathBuf::from("C:/Users/x")
                .join("AppData")
                .join("Local")
                .join("Iris")
                .join("models")
        );
        assert_eq!(
            resolve_model_dir(None, None, None, Some("/home/x")),
            PathBuf::from("/home/x").join(".cache").join("iris").join("models")
        );
        assert_eq!(
            resolve_model_dir(None, None, None, None),
            PathBuf::from(".iris-models")
        );
        assert_eq!(
            resolve_model_dir(Some("  "), None, None, None),
            PathBuf::from(".iris-models")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_model_dir_uses_home_only_on_unix() {
        assert_eq!(
            resolve_model_dir(Some("/custom"), Some("/home/x")),
            PathBuf::from("/custom")
        );
        assert_eq!(
            resolve_model_dir(None, Some("/home/x")),
            PathBuf::from("/home/x").join(".cache").join("iris").join("models")
        );
        assert_eq!(
            resolve_model_dir(None, None),
            PathBuf::from(".iris-models")
        );
        assert_eq!(
            resolve_model_dir(Some("  "), None),
            PathBuf::from(".iris-models")
        );
        // Inherited Windows env vars are not consulted on Unix (see default_model_dir).
        assert_eq!(
            resolve_model_dir(None, Some("/home/x")),
            PathBuf::from("/home/x").join(".cache").join("iris").join("models")
        );
    }

    #[test]
    fn verify_existing_rejects_wrong_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ModelCatalog::get(ModelId::ZipformerTokens);
        let dest = dir.path().join(spec.relative_path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"too-small").unwrap();
        let err = verify_existing(&dest, spec).unwrap_err();
        assert!(
            err.to_string().contains("size") || err.to_string().contains("expected"),
            "{err}"
        );
    }

    #[test]
    fn ensure_model_removes_corrupt_cache_before_redownload() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ModelCatalog::get(ModelId::ZipformerTokens);
        let dest = dir.path().join(spec.relative_path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"too-small").unwrap();
        // Offline or online: corrupt entry must not remain after ensure_model.
        let result = ensure_model(ModelId::ZipformerTokens, dir.path(), None);
        match result {
            Ok(path) => {
                assert_eq!(path, dest);
                verify_existing(&dest, spec).unwrap();
            }
            Err(_) => {
                assert!(
                    !dest.is_file(),
                    "corrupt model file must be removed when re-download fails"
                );
            }
        }
    }
}
