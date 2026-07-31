//! Shared async plumbing for the network engines.
//!
//! One process-wide Tokio runtime, created on first use. Two worker threads is
//! plenty: the only work on it is a TLS websocket and an HTTP request. Building
//! a runtime per dictation would put ~1 ms of thread spawning on the key-press
//! path for no benefit.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> Result<&'static Runtime> {
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("iris-net")
        .build()
        .context("building the Tokio runtime")?;
    // A racing initialiser just means we drop ours and use theirs.
    let _ = RUNTIME.set(rt);
    Ok(RUNTIME.get().expect("runtime just set"))
}

/// Install the `ring` crypto provider for rustls.
///
/// rustls 0.23 requires a process-wide provider and refuses to guess when more
/// than one is compiled in. Calling this more than once is fine; a second call
/// returns `Err` from rustls and we ignore it.
pub fn init_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
