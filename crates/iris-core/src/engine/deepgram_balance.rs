//! Reading a Deepgram project's remaining balance from Deepgram's
//! **Management API** — a different surface from [`super::deepgram`], which
//! only ever streams audio to the transcription websocket.
//!
//! # Why this needs a second, separate key
//!
//! The transcription key `DeepgramEngine` uses only has to be good for
//! opening a websocket; Deepgram's Management API additionally requires the
//! key to carry a `billing:read` scope (Deepgram's own docs on
//! [roles and API scopes](https://developers.deepgram.com/guides/deep-dives/working-with-roles),
//! checked 2026-08-13) — a project-scoped permission most transcription keys
//! do not have, and should not need, since the two purposes have nothing to
//! do with each other. So this module never reuses `IRIS_DEEPGRAM_KEY`; it
//! reads its own, entirely optional key (see [`key_from_env`]), and every
//! caller of [`fetch`] must treat its absence as "the feature is off", never
//! as an error.
//!
//! # The endpoints
//!
//! There is no "my balance" endpoint that takes only a key — every balance
//! endpoint is nested under a project id
//! (<https://developers.deepgram.com/reference/manage/billing/list>, checked
//! 2026-08-13). So [`fetch`] first resolves the account's project with
//! [List Projects](https://developers.deepgram.com/reference/get-projects)
//! (`GET /v1/projects`, checked 2026-08-13) — which needs no id itself, only
//! the key — and takes its first entry: Iris has no concept of "which
//! project", and a personal Deepgram account backing one dictation app has
//! exactly one. It then calls
//! [Get Project Balances](https://developers.deepgram.com/reference/manage/billing/list)
//! (`GET /v1/projects/{project_id}/balances`) and sums every entry's
//! `amount` — an account can carry more than one outstanding balance (e.g. a
//! purchased block plus pay-as-you-go), and what the user actually wants to
//! know is the total left to spend, not which bucket it sits in. Every
//! request authenticates with `Authorization: Token <key>` — Deepgram's own
//! scheme, not `Bearer` (confirmed against the docs above; using `Bearer`
//! here would silently produce a `401` on every call).
//!
//! # Never call this from the dictation path
//!
//! [`fetch`] blocks its caller on a real HTTP round trip. That is fine for
//! the background thread `iris-app`'s balance monitor gives it and nowhere
//! else — see that module's doc comment for the scheduling rule (on demand,
//! at most on a slow timer, never per dictation).
//!
//! # Never leaks the key
//!
//! Like [`super::deepgram`] and [`super::groq`], nothing here ever logs,
//! echoes or formats the key: every classification below reads only an HTTP
//! status code or an I/O error's own text, never the request that carried
//! the key or a response body that could not possibly echo it back.

use std::time::Duration;

use serde::Deserialize;

use super::{net, FailureCause};

/// Environment variable carrying the optional Management API key. Never
/// promoted anywhere but read directly here, the same "environment only"
/// rule [`super::require_key`] already follows for the transcription keys —
/// see `iris_app::config::Keys::deepgram_management`'s doc comment for how a
/// value in `config.toml` reaches this variable.
pub const KEY_ENV: &str = "IRIS_DEEPGRAM_MANAGEMENT_KEY";

/// Where to fix a rejected or scope-less key.
pub const CONSOLE_URL: &str = "https://console.deepgram.com";

const PROVIDER: &str = "Deepgram";

/// How long the whole two-request round trip (projects, then balances) may
/// take before giving up. Generous compared to the transcription engines'
/// own budgets — this runs on a background thread with nothing waiting on
/// it, so there is no latency budget to protect, only a promise that a dead
/// connection does not hang the thread forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const PROJECTS_URL: &str = "https://api.deepgram.com/v1/projects";

/// Read the optional Management API key from the environment. `None` when
/// unset or blank, which every caller must treat as "the balance feature is
/// off" — never as a reason to show an error, per this crate's module docs.
#[must_use]
pub fn key_from_env() -> Option<String> {
    std::env::var(KEY_ENV)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// A project's total remaining balance — see the module docs for why this is
/// a sum across every balance entry Deepgram reports, not a single figure.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    /// The total amount left across every outstanding balance entry.
    pub amount: f64,
    /// The currency Deepgram reported (every documented example is `"USD"`).
    /// Empty only if the account has no outstanding balance entries at all,
    /// which this module reads as a real, reportable `amount: 0.0` rather
    /// than an error — see [`balances_for`].
    pub units: String,
}

/// Why [`fetch`] could not produce a [`Balance`]. Never carries the key —
/// see the module docs.
#[derive(Debug)]
pub enum BalanceError {
    /// A specific, actionable cause, in the same vocabulary
    /// [`FailureCause`] already gives a failed dictation. `detail` is the
    /// same kind of extra evidence `FailureCause::message` already accepts
    /// (an HTTP status, an I/O error's own text) — never the key.
    Classified {
        cause: FailureCause,
        detail: Option<String>,
    },
    /// Anything else this module cannot confidently classify: a malformed
    /// response, a key with no projects on it, a JSON shape Deepgram's docs
    /// do not describe. Free text, but always built from the response's own
    /// shape or an I/O error's own message — never the key.
    Other(String),
}

impl std::fmt::Display for BalanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BalanceError::Classified { cause, detail } => write!(
                f,
                "{}",
                cause.message(PROVIDER, KEY_ENV, CONSOLE_URL, detail.as_deref())
            ),
            BalanceError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

/// Fetch the account's total remaining balance, blocking the calling thread
/// for the network round trip. See the module docs for why this must never
/// be called from anywhere on the dictation path.
///
/// # Errors
///
/// See [`BalanceError`].
pub fn fetch(management_key: &str) -> Result<Balance, BalanceError> {
    net::init_crypto();
    let rt = net::runtime().map_err(|e| BalanceError::Other(format!("{e:#}")))?;
    rt.block_on(fetch_async(management_key))
}

async fn fetch_async(key: &str) -> Result<Balance, BalanceError> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| BalanceError::Other(format!("building the HTTP client: {e}")))?;

    let project_id = first_project_id(&client, key).await?;
    let url = format!("{PROJECTS_URL}/{project_id}/balances");
    let balances: BalancesResponse = get_json(&client, &url, key).await?;
    Ok(sum_balances(balances))
}

#[derive(Debug, Deserialize)]
struct ProjectsResponse {
    projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
struct ProjectEntry {
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct BalancesResponse {
    balances: Vec<BalanceEntry>,
}

#[derive(Debug, Deserialize)]
struct BalanceEntry {
    amount: f64,
    units: String,
}

/// A balance-less response reads as a real `0.0`, not a lookup failure — the
/// documented shape has no "no outstanding balance" sentinel distinct from
/// an empty list, and an account that has genuinely spent everything is
/// exactly the case this feature most needs to be able to show, not swallow
/// as "unknown". Takes `units` from the first entry, defaulting to `"USD"`
/// (every documented example) when there is nothing to read it from.
fn sum_balances(response: BalancesResponse) -> Balance {
    let units = response
        .balances
        .first()
        .map(|b| b.units.clone())
        .unwrap_or_else(|| "USD".to_string());
    let amount = response.balances.iter().map(|b| b.amount).sum();
    Balance { amount, units }
}

async fn first_project_id(client: &reqwest::Client, key: &str) -> Result<String, BalanceError> {
    let projects: ProjectsResponse = get_json(client, PROJECTS_URL, key).await?;
    projects
        .projects
        .into_iter()
        .next()
        .map(|p| p.project_id)
        .ok_or_else(|| {
            BalanceError::Other("this Deepgram key is not associated with any project".to_string())
        })
}

/// `GET url` with `Authorization: Token <key>`, classifying a non-2xx status
/// and returning the parsed body otherwise. Shared by both requests
/// [`fetch_async`] makes.
async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    key: &str,
) -> Result<T, BalanceError> {
    let response = match client
        .get(url)
        .header("Authorization", format!("Token {key}"))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(classify_send_error(&e)),
    };
    let status = response.status();
    if !status.is_success() {
        return Err(BalanceError::Classified {
            cause: classify_response_status(status.as_u16()),
            detail: Some(format!("HTTP {}", status.as_u16())),
        });
    }
    response
        .json::<T>()
        .await
        .map_err(|e| BalanceError::Other(format!("parsing the Deepgram response: {e}")))
}

/// Classify a Management API rejection by its HTTP status, per the same
/// documented error codes [`super::deepgram`]'s own classifier cites
/// (<https://developers.deepgram.com/docs/errors>, checked 2026-08-13),
/// plus the scope requirement the Management API layers on top
/// (<https://developers.deepgram.com/guides/deep-dives/working-with-roles>,
/// checked 2026-08-13):
///
/// - `401` — the key itself is invalid.
/// - `403` — a key that is valid but lacks the `billing:read` (or
///   `project:read`) scope this endpoint needs. Folded into the same
///   [`FailureCause::InvalidKey`] as `401`: the fix is identical either way
///   — check the key and its scopes in the console.
/// - `429` — rate limited.
///
/// This endpoint has no documented `402`/exhausted-credit status of its own
/// — that would be a strange thing for a *balance-reading* endpoint to
/// return — so unlike [`super::deepgram`]'s classifier there is no
/// [`FailureCause::ExhaustedCredit`] case here. Every other status is
/// [`FailureCause::Unknown`], read from the status code alone, never a
/// response body Deepgram's docs do not describe.
fn classify_response_status(status: u16) -> FailureCause {
    match status {
        401 | 403 => FailureCause::InvalidKey,
        429 => FailureCause::RateLimited,
        _ => FailureCause::Unknown,
    }
}

/// Classify a failure to even get a response, the same predicates
/// [`super::groq`]'s own `classify_send_error` uses — `reqwest::Error`'s
/// public `is_timeout`/`is_connect`.
fn classify_send_error(err: &reqwest::Error) -> BalanceError {
    let cause = if err.is_timeout() {
        FailureCause::Timeout
    } else if err.is_connect() {
        FailureCause::NetworkUnreachable
    } else {
        return BalanceError::Other(format!("reaching Deepgram: {err}"));
    };
    BalanceError::Classified {
        cause,
        detail: Some(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_env_is_none_when_unset_or_blank() {
        // Guarded by a lock across every test in this file that touches
        // `std::env`, so parallel test threads cannot see one another's
        // value for the same variable.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(KEY_ENV);
        assert_eq!(key_from_env(), None);
        std::env::set_var(KEY_ENV, "   ");
        assert_eq!(key_from_env(), None);
        std::env::remove_var(KEY_ENV);
    }

    #[test]
    fn key_from_env_trims_a_real_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(KEY_ENV, "  secret-management-key  ");
        assert_eq!(key_from_env(), Some("secret-management-key".to_string()));
        std::env::remove_var(KEY_ENV);
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn status_codes_classify_per_the_documented_meaning() {
        assert_eq!(classify_response_status(401), FailureCause::InvalidKey);
        assert_eq!(classify_response_status(403), FailureCause::InvalidKey);
        assert_eq!(classify_response_status(429), FailureCause::RateLimited);
        assert_eq!(classify_response_status(500), FailureCause::Unknown);
        assert_eq!(classify_response_status(402), FailureCause::Unknown);
    }

    #[test]
    fn sum_balances_adds_every_entry_and_reads_units_from_the_first() {
        let response = BalancesResponse {
            balances: vec![
                BalanceEntry {
                    amount: 3.5,
                    units: "USD".into(),
                },
                BalanceEntry {
                    amount: 1.25,
                    units: "USD".into(),
                },
            ],
        };
        let balance = sum_balances(response);
        assert_eq!(balance.amount, 4.75);
        assert_eq!(balance.units, "USD");
    }

    #[test]
    fn sum_balances_of_an_empty_list_is_a_real_zero_not_an_error() {
        let balance = sum_balances(BalancesResponse { balances: vec![] });
        assert_eq!(balance.amount, 0.0);
        assert_eq!(balance.units, "USD");
    }

    /// A real local connection refusal, the same technique
    /// `groq.rs`'s own `NetworkUnreachable` test uses — a `TcpListener` bound
    /// and immediately dropped leaves the port refusing connections, so this
    /// exercises the real `reqwest::Error` shape rather than a synthetic one.
    #[test]
    fn a_connection_refusal_classifies_as_network_unreachable_and_never_leaks_the_key() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let key = "sk-super-secret-management-key";

        net::init_crypto();
        let rt = net::runtime().unwrap();
        let result = rt.block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            get_json::<ProjectsResponse>(&client, &format!("http://{addr}/v1/projects"), key).await
        });

        match result {
            Err(BalanceError::Classified {
                cause: FailureCause::NetworkUnreachable,
                detail,
            }) => {
                let detail = detail.unwrap_or_default();
                assert!(!detail.contains(key), "leaked the key: {detail}");
            }
            other => panic!("expected a classified NetworkUnreachable error, got {other:?}"),
        }
    }

    /// A worst-case, misbehaving server: echoes the whole raw request —
    /// `Authorization` header, key included — back as the response body of a
    /// `401`. `get_json` must still never leak it, because
    /// `classify_response_status` reads only the status line and the body is
    /// never inspected on a non-success response. Proves that by
    /// construction rather than by trusting it: an adversarial body is the
    /// only way to be sure nothing quietly starts reading it later.
    #[test]
    fn a_response_that_echoes_the_key_in_its_body_never_leaks_it_into_the_final_message() {
        use std::io::{Read, Write};

        let key = "sk-super-secret-management-key";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let echoed = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                echoed.len(),
                echoed
            );
            let _ = stream.write_all(response.as_bytes());
        });

        net::init_crypto();
        let rt = net::runtime().unwrap();
        let result = rt.block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            get_json::<ProjectsResponse>(&client, &format!("http://{addr}/v1/projects"), key).await
        });
        server.join().unwrap();

        let message = match result {
            Err(
                e @ BalanceError::Classified {
                    cause: FailureCause::InvalidKey,
                    ..
                },
            ) => e.to_string(),
            other => panic!("expected a classified InvalidKey error, got {other:?}"),
        };
        assert!(!message.contains(key), "leaked the key: {message}");
    }
}
