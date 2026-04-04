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

    // Ollama uses a non-OpenAI endpoint for model listing
    let (url, is_ollama) = if lower == "ollama" {
        // base_url is typically "http://localhost:11434/v1" — strip /v1 for the ps endpoint
        let root = base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches("/v1/");
        // Use /api/ps to list only currently running/loaded models
        (format!("{root}/api/ps"), true)
    } else {
        // OpenAI-compatible: GET {base_url}/models
        let trimmed = base_url.trim_end_matches('/');
        (format!("{trimmed}/models"), false)
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                latency_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("{e}")),
                ..Default::default()
            };
        }
    };

    if !resp.status().is_success() {
        return ProbeResult {
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(format!("HTTP {}", resp.status())),
            ..Default::default()
        };
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult {
                reachable: true, // server responded, just bad JSON
                latency_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Invalid JSON: {e}")),
                ..Default::default()
            };
        }
    };

    let latency_ms = start.elapsed().as_millis() as u64;

    // Parse model names
    let model_names: Vec<String> = if is_ollama {
        // Ollama /api/ps: { "models": [ { "name": "model:latest", ... }, ... ] }
        body.get("models")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        // OpenAI-compatible: { "data": [ { "id": "model-name", ... }, ... ] }
        body.get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|n| n.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };

    // For Ollama, enrich each model with actual context window from /api/show
    let models = if is_ollama {
        let root = base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches("/v1/");
        let show_url = format!("{root}/api/show");
        let mut enriched = Vec::with_capacity(model_names.len());
        for name in &model_names {
            let ctx = fetch_ollama_context_window(&client, &show_url, name).await;
            enriched.push(DiscoveredModel {
                id: name.clone(),
                context_window: ctx,
            });
        }
        enriched
    } else {
        model_names
            .into_iter()
            .map(|id| DiscoveredModel {
                id,
                context_window: None,
            })
            .collect()
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
