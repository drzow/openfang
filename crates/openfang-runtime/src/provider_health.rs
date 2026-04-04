//! Provider health probing — lightweight HTTP checks for local LLM providers.
//!
//! Probes local providers (Ollama, vLLM, LM Studio) for reachability and
//! dynamically discovers which models they currently serve.
//!
//! Includes a [`ProbeCache`] with configurable TTL so that the `/api/providers`
//! endpoint returns instantly on repeated dashboard loads instead of blocking
//! on TCP connect timeouts to unreachable local services.

use dashmap::DashMap;
use std::time::{Duration, Instant};

/// A model discovered from a local provider with optional runtime metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveredModel {
    /// Model identifier (e.g. `"llama3.2:latest"`).
    pub id: String,
    /// Actual runtime context window in tokens (e.g. from Ollama's `num_ctx`),
    /// or `None` if the provider does not report it.
    pub context_window: Option<u64>,
}

/// Result of probing a provider endpoint.
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    /// Whether the provider responded successfully.
    pub reachable: bool,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Models discovered from the provider's listing endpoint.
    pub discovered_models: Vec<DiscoveredModel>,
    /// Error message if the probe failed.
    pub error: Option<String>,
}

impl ProbeResult {
    /// Model IDs as plain strings (convenience accessor).
    pub fn model_ids(&self) -> Vec<String> {
        self.discovered_models.iter().map(|m| m.id.clone()).collect()
    }
}

/// Check if a provider is a local provider (no key required, localhost URL).
///
/// Returns true for `"ollama"`, `"vllm"`, `"lmstudio"`.
pub fn is_local_provider(provider: &str) -> bool {
    matches!(
        provider.to_lowercase().as_str(),
        "ollama" | "vllm" | "lmstudio"
    )
}

/// Overall request timeout for local provider health probes (connect + response).
const PROBE_TIMEOUT_SECS: u64 = 2;

/// TCP connect timeout — fail fast when the local port is not listening.
const PROBE_CONNECT_TIMEOUT_SECS: u64 = 1;

/// Default TTL for cached probe results (seconds).
const PROBE_CACHE_TTL_SECS: u64 = 60;

// ── Probe cache ──────────────────────────────────────────────────────────

/// Thread-safe cache for provider probe results.
///
/// Entries expire after [`PROBE_CACHE_TTL_SECS`] seconds. The cache is
/// designed to be stored once in `AppState` and shared across requests.
pub struct ProbeCache {
    inner: DashMap<String, (Instant, ProbeResult)>,
    ttl: Duration,
}

impl ProbeCache {
    /// Create a new cache with the default 60-second TTL.
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            ttl: Duration::from_secs(PROBE_CACHE_TTL_SECS),
        }
    }

    /// Look up a cached probe result. Returns `None` if missing or expired.
    pub fn get(&self, provider_id: &str) -> Option<ProbeResult> {
        if let Some(entry) = self.inner.get(provider_id) {
            let (ts, ref result) = *entry;
            if ts.elapsed() < self.ttl {
                return Some(result.clone());
            }
            // Expired — drop the read guard before removing
            drop(entry);
            self.inner.remove(provider_id);
        }
        None
    }

    /// Store a probe result.
    pub fn insert(&self, provider_id: &str, result: ProbeResult) {
        self.inner
            .insert(provider_id.to_string(), (Instant::now(), result));
    }
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Probe a provider's health by hitting its model listing endpoint.
///
/// - **Ollama**: `GET {base_url_root}/api/tags` → parses `.models[].name`
/// - **OpenAI-compat** (vLLM, LM Studio): `GET {base_url}/models` → parses `.data[].id`
///
/// `base_url` should be the provider's base URL from the catalog (e.g.,
/// `http://localhost:11434/v1` for Ollama, `http://localhost:8000/v1` for vLLM).
pub async fn probe_provider(provider: &str, base_url: &str) -> ProbeResult {
    let start = Instant::now();

    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(PROBE_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                error: Some(format!("Failed to build HTTP client: {e}")),
                ..Default::default()
            };
        }
    };

    let lower = provider.to_lowercase();
    let root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches("/v1/");
    let trimmed = base_url.trim_end_matches('/');

    // ── Step 1: Discover models ─────────────────────────────────────────────
    // For Ollama-configured providers, try endpoints in order of preference:
    //   1. /api/ps   (Ollama proper — only running models)
    //   2. /api/tags (Ollama-compat / llama.cpp — all loaded models)
    //   3. /v1/models (OpenAI-compat — universal fallback)
    // For non-Ollama providers, go straight to /v1/models.

    let body: serde_json::Value;
    let mut used_openai_compat = false;

    if lower == "ollama" {
        // Try /api/ps first (running models only)
        let ps_url = format!("{root}/api/ps");
        let ps_result = client.get(&ps_url).send().await;
        if let Ok(resp) = ps_result {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    body = v;
                } else {
                    body = serde_json::Value::Null;
                }
            } else {
                // /api/ps failed (e.g. 404 on llama.cpp) — try /api/tags
                let tags_url = format!("{root}/api/tags");
                if let Ok(resp2) = client.get(&tags_url).send().await {
                    if resp2.status().is_success() {
                        body = resp2.json().await.unwrap_or(serde_json::Value::Null);
                    } else {
                        // Fall back to OpenAI-compat /v1/models
                        let models_url = format!("{trimmed}/models");
                        match client.get(&models_url).send().await {
                            Ok(r) if r.status().is_success() => {
                                used_openai_compat = true;
                                body = r.json().await.unwrap_or(serde_json::Value::Null);
                            }
                            _ => {
                                return ProbeResult {
                                    latency_ms: start.elapsed().as_millis() as u64,
                                    error: Some("All Ollama endpoints failed".into()),
                                    ..Default::default()
                                };
                            }
                        }
                    }
                } else {
                    body = serde_json::Value::Null;
                }
            }
        } else {
            return ProbeResult {
                latency_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("{}", ps_result.unwrap_err())),
                ..Default::default()
            };
        }
    } else {
        // Non-Ollama: GET {base_url}/models
        let models_url = format!("{trimmed}/models");
        match client.get(&models_url).send().await {
            Ok(r) if r.status().is_success() => {
                used_openai_compat = true;
                body = r.json().await.unwrap_or(serde_json::Value::Null);
            }
            Ok(r) => {
                return ProbeResult {
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("HTTP {}", r.status())),
                    ..Default::default()
                };
            }
            Err(e) => {
                return ProbeResult {
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("{e}")),
                    ..Default::default()
                };
            }
        }
    }

    if body.is_null() {
        return ProbeResult {
            reachable: true,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some("Server responded but returned invalid JSON".into()),
            ..Default::default()
        };
    }

    let latency_ms = start.elapsed().as_millis() as u64;

    // ── Step 2: Parse model names and inline context metadata ───────────────

    let models: Vec<DiscoveredModel> = if used_openai_compat {
        // OpenAI-compatible: { "data": [ { "id": "model-name", "meta": { "n_ctx_train": N }, ... } ] }
        body.get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(|n| n.as_str())?.to_string();
                        // llama.cpp includes meta.n_ctx_train in /v1/models
                        let ctx_train = m
                            .get("meta")
                            .and_then(|meta| meta.get("n_ctx_train"))
                            .and_then(|v| v.as_u64());
                        Some(DiscoveredModel {
                            id,
                            context_window: ctx_train,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        // Ollama /api/ps or /api/tags: { "models": [ { "name": "model:latest", ... } ] }
        body.get("models")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let name = m.get("name").and_then(|n| n.as_str())?.to_string();
                        Some(DiscoveredModel {
                            id: name,
                            context_window: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    // ── Step 3: Enrich with runtime context window ──────────────────────────
    // Try /props (llama.cpp) for actual n_ctx, then /api/show (Ollama) per model.
    let models = if lower == "ollama" {
        let mut enriched = models;
        // First try /props — llama.cpp exposes actual runtime n_ctx here
        let props_ctx = fetch_props_n_ctx(&client, root).await;
        if let Some(n_ctx) = props_ctx {
            // /props returns the server-wide n_ctx — apply to all models
            for m in &mut enriched {
                if m.context_window.is_none() || m.context_window == Some(0) {
                    m.context_window = Some(n_ctx);
                }
                // If we got n_ctx_train from /v1/models but /props has the real runtime value, prefer it
                if let Some(train) = m.context_window {
                    if train > n_ctx {
                        m.context_window = Some(n_ctx);
                    }
                }
            }
        } else {
            // Fall back to /api/show per model (Ollama proper)
            let show_url = format!("{root}/api/show");
            for m in &mut enriched {
                if m.context_window.is_none() {
                    m.context_window =
                        fetch_ollama_context_window(&client, &show_url, &m.id).await;
                }
            }
        }
        enriched
    } else {
        models
    };

    ProbeResult {
        reachable: true,
        latency_ms,
        discovered_models: models,
        error: None,
    }
}

/// Query Ollama's `/api/show` for a model's runtime context window.
///
/// Checks the `parameters` text for `num_ctx` (the actual runtime value),
/// then falls back to `model_info.<arch>.context_length` (the model's max).
/// Returns `None` if the call fails or neither field is found.
async fn fetch_ollama_context_window(
    client: &reqwest::Client,
    show_url: &str,
    model_name: &str,
) -> Option<u64> {
    let body = serde_json::json!({"name": model_name});
    let resp = client.post(show_url).json(&body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    parse_ollama_context_window(&json)
}

/// Query a llama.cpp server's `/props` endpoint for the runtime `n_ctx`.
///
/// llama.cpp exposes `default_generation_settings.n_ctx` which is the actual
/// context window configured at server start (may be much smaller than
/// the model's training context). Returns `None` if the endpoint is unavailable.
async fn fetch_props_n_ctx(client: &reqwest::Client, root_url: &str) -> Option<u64> {
    let url = format!("{root_url}/props");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("default_generation_settings")
        .and_then(|dgs| dgs.get("n_ctx"))
        .and_then(|v| v.as_u64())
}

/// Parse the context window from an Ollama `/api/show` response.
///
/// Priority:
/// 1. `parameters` string containing `num_ctx <value>` (runtime config)
/// 2. `model_info.<arch>.context_length` (model's theoretical max)
fn parse_ollama_context_window(json: &serde_json::Value) -> Option<u64> {
    // Primary: parse num_ctx from the parameters text
    if let Some(params) = json.get("parameters").and_then(|v| v.as_str()) {
        for line in params.lines() {
            let line = line.trim();
            if line.starts_with("num_ctx") {
                if let Some(val) = line.split_whitespace().nth(1) {
                    if let Ok(n) = val.parse::<u64>() {
                        return Some(n);
                    }
                }
            }
        }
    }

    // Fallback: model_info.<arch>.context_length
    if let Some(info) = json.get("model_info").and_then(|v| v.as_object()) {
        for (key, val) in info {
            if key.ends_with(".context_length") {
                if let Some(n) = val.as_u64() {
                    return Some(n);
                }
            }
        }
    }

    None
}

/// Probe a provider, returning a cached result when available.
///
/// If the cache contains a non-expired entry the HTTP request is skipped
/// entirely, making repeated `/api/providers` calls instantaneous.
pub async fn probe_provider_cached(
    provider: &str,
    base_url: &str,
    cache: &ProbeCache,
) -> ProbeResult {
    if let Some(cached) = cache.get(provider) {
        return cached;
    }
    let result = probe_provider(provider, base_url).await;
    cache.insert(provider, result.clone());
    result
}

/// Lightweight model probe -- sends a minimal completion request to verify a model is responsive.
///
/// Unlike `probe_provider` which checks the listing endpoint, this actually sends
/// a tiny prompt ("Hi") to verify the model can generate completions. Used by the
/// circuit breaker to re-test a provider during cooldown.
///
/// Returns `Ok(latency_ms)` if the model responds, or `Err(error_message)` if it fails.
pub async fn probe_model(
    provider: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> Result<u64, String> {
    let start = Instant::now();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Hi"}],
        "max_tokens": 1,
        "temperature": 0.0
    });

    let mut req = client.post(&url).json(&body);
    if let Some(key) = api_key {
        // Detect provider to set correct auth header
        let lower = provider.to_lowercase();
        if lower == "gemini" {
            req = req.header("x-goog-api-key", key);
        } else {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
    }

    let resp = req.send().await.map_err(|e| format!("{e}"))?;
    let latency = start.elapsed().as_millis() as u64;

    if resp.status().is_success() {
        Ok(latency)
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "HTTP {status}: {}",
            crate::str_utils::safe_truncate_str(&body, 200)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_local_provider_true_for_ollama() {
        assert!(is_local_provider("ollama"));
        assert!(is_local_provider("Ollama"));
        assert!(is_local_provider("OLLAMA"));
        assert!(is_local_provider("vllm"));
        assert!(is_local_provider("lmstudio"));
    }

    #[test]
    fn test_is_local_provider_false_for_openai() {
        assert!(!is_local_provider("openai"));
        assert!(!is_local_provider("anthropic"));
        assert!(!is_local_provider("gemini"));
        assert!(!is_local_provider("groq"));
    }

    #[test]
    fn test_probe_result_default() {
        let result = ProbeResult::default();
        assert!(!result.reachable);
        assert_eq!(result.latency_ms, 0);
        assert!(result.discovered_models.is_empty());
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_probe_unreachable_returns_error() {
        // Probe a port that's almost certainly not running a server
        let result = probe_provider("ollama", "http://127.0.0.1:19999").await;
        assert!(!result.reachable);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_probe_timeout_value() {
        assert_eq!(PROBE_TIMEOUT_SECS, 2);
        assert_eq!(PROBE_CONNECT_TIMEOUT_SECS, 1);
    }

    #[test]
    fn test_probe_model_url_construction() {
        // Verify the URL format logic used inside probe_model.
        let url = format!(
            "{}/chat/completions",
            "http://localhost:8000/v1".trim_end_matches('/')
        );
        assert_eq!(url, "http://localhost:8000/v1/chat/completions");

        let url2 = format!(
            "{}/chat/completions",
            "http://localhost:8000/v1/".trim_end_matches('/')
        );
        assert_eq!(url2, "http://localhost:8000/v1/chat/completions");
    }

    #[tokio::test]
    async fn test_probe_model_unreachable() {
        let result = probe_model("test", "http://127.0.0.1:19998/v1", "test-model", None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_probe_cache_miss_returns_none() {
        let cache = ProbeCache::new();
        assert!(cache.get("ollama").is_none());
    }

    #[test]
    fn test_probe_cache_hit_returns_result() {
        let cache = ProbeCache::new();
        let result = ProbeResult {
            reachable: true,
            latency_ms: 42,
            discovered_models: vec![DiscoveredModel {
                id: "llama3".into(),
                context_window: Some(4096),
            }],
            error: None,
        };
        cache.insert("ollama", result.clone());
        let cached = cache.get("ollama").expect("should be cached");
        assert!(cached.reachable);
        assert_eq!(cached.latency_ms, 42);
        assert_eq!(cached.discovered_models.len(), 1);
        assert_eq!(cached.discovered_models[0].id, "llama3");
        assert_eq!(cached.discovered_models[0].context_window, Some(4096));
    }

    #[test]
    fn test_probe_cache_default() {
        let cache = ProbeCache::default();
        assert!(cache.get("anything").is_none());
        assert_eq!(cache.ttl, Duration::from_secs(PROBE_CACHE_TTL_SECS));
    }

    #[test]
    fn test_probe_result_model_ids() {
        let result = ProbeResult {
            reachable: true,
            latency_ms: 10,
            discovered_models: vec![
                DiscoveredModel { id: "a".into(), context_window: Some(4096) },
                DiscoveredModel { id: "b".into(), context_window: None },
            ],
            error: None,
        };
        assert_eq!(result.model_ids(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_parse_ollama_num_ctx_from_parameters() {
        let json = serde_json::json!({
            "parameters": "num_ctx                        4096\ntemperature                    0.7"
        });
        assert_eq!(parse_ollama_context_window(&json), Some(4096));
    }

    #[test]
    fn test_parse_ollama_context_length_fallback() {
        let json = serde_json::json!({
            "model_info": { "qwen2.context_length": 32768 }
        });
        assert_eq!(parse_ollama_context_window(&json), Some(32768));
    }

    #[test]
    fn test_parse_ollama_num_ctx_takes_priority() {
        let json = serde_json::json!({
            "parameters": "num_ctx 4096",
            "model_info": { "llama.context_length": 131072 }
        });
        // num_ctx (runtime) takes priority over model_info (theoretical max)
        assert_eq!(parse_ollama_context_window(&json), Some(4096));
    }

    #[test]
    fn test_parse_ollama_no_context_info() {
        let json = serde_json::json!({"parameters": "temperature 0.7"});
        assert_eq!(parse_ollama_context_window(&json), None);
    }

    #[test]
    fn test_parse_ollama_empty_response() {
        let json = serde_json::json!({});
        assert_eq!(parse_ollama_context_window(&json), None);
    }
}
