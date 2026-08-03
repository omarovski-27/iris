//! Shared async plumbing for the network engines.
//!
//! One process-wide Tokio runtime, created on first use. Two worker threads is
//! plenty: the only work on it is a TLS websocket and an HTTP request. Building
//! a runtime per dictation would put ~1 ms of thread spawning on the key-press
//! path for no benefit.
//!
//! # Sharing the TLS config for session resumption
//!
//! `tokio_tungstenite::connect_async` — what `deepgram.rs` used to call
//! directly — builds a brand-new `rustls::ClientConfig` internally on every
//! call when no connector is supplied (see its `tls.rs`: a fresh
//! `RootCertStore` and `ClientConfig::builder()...with_no_client_auth()` each
//! time). rustls enables session-ticket resumption by default, but the cache
//! lives on the `ClientConfig` instance itself, so a fresh config every
//! dictation means a fresh, empty cache every dictation — every single
//! connect pays a full handshake, never a resumed one.
//!
//! [`tls_connector`] builds that same default config exactly once (identical
//! root store, identical `webpki-roots` version tokio-tungstenite itself
//! pins) and hands out a cloned `Arc` to it. Reusing the same `Arc` across
//! connects to the same host (Deepgram) is what lets rustls's client-side
//! resumption actually engage on the second and later connection in a
//! process's lifetime.
//!
//! Live-measured against the real `api.deepgram.com` endpoint (a raw
//! TLS+HTTP round trip, not authenticated Deepgram traffic — verifying this
//! needs no API key): resumption *does* engage — `ClientConnection`'s
//! `handshake_kind()` reports `Resumed` on the second and later connect —
//! but the wall-clock TLS handshake time barely moved (~205-207ms either
//! way, well within run-to-run noise). That is expected, not a bug: TLS 1.3's
//! *full* handshake is already 1-RTT, so resumption without 0-RTT (early
//! data — not implemented here) saves the server-side certificate
//! verification and asymmetric key exchange compute, not a round trip. This
//! is a real but modest win, not the fix for `stream_ready_ms`'s dominant
//! cost — see [`super::deepgram`]'s `WarmPool` for that. It changes nothing
//! about *when* or *how often* Iris connects — every dictation still opens a
//! fresh session exactly as before — only how much a fresh connect costs, so
//! it carries none of the staleness/data-loss risk a reused *connection*
//! would.
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use rustls::{ClientConfig, RootCertStore};
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

static TLS_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

/// A shared rustls client config for websocket connects, so repeat
/// connections to the same host can resume a TLS session instead of paying a
/// full handshake. See the module doc.
pub fn tls_connector() -> Arc<ClientConfig> {
    init_crypto();
    TLS_CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
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
