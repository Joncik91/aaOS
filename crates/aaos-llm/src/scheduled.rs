use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};

use crate::client::LlmClient;
use crate::error::{LlmError, LlmResult};
use crate::types::{CompletionRequest, CompletionResponse};

/// Configuration for inference scheduling.
#[derive(Debug, Clone)]
pub struct InferenceSchedulingConfig {
    /// Maximum concurrent LLM API calls. Default: 3.
    pub max_concurrent: usize,
    /// Minimum milliseconds between consecutive API calls. Default: 0 (no delay).
    pub min_delay_ms: u64,
}

impl Default for InferenceSchedulingConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            min_delay_ms: 0,
        }
    }
}

impl InferenceSchedulingConfig {
    /// Load from environment variables.
    /// AAOS_MAX_CONCURRENT_INFERENCE (default 3)
    /// AAOS_MIN_INFERENCE_DELAY_MS (default 0)
    pub fn from_env() -> Self {
        let max_concurrent = std::env::var("AAOS_MAX_CONCURRENT_INFERENCE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let min_delay_ms = std::env::var("AAOS_MIN_INFERENCE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Self {
            max_concurrent,
            min_delay_ms,
        }
    }
}

/// LlmClient decorator that limits concurrent API calls via a semaphore
/// and optionally enforces a minimum delay between calls.
pub struct ScheduledLlmClient {
    inner: Arc<dyn LlmClient>,
    semaphore: Arc<Semaphore>,
    min_delay: Duration,
    last_call: Mutex<Instant>,
}

impl ScheduledLlmClient {
    pub fn new(inner: Arc<dyn LlmClient>, config: InferenceSchedulingConfig) -> Self {
        Self {
            inner,
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            min_delay: Duration::from_millis(config.min_delay_ms),
            last_call: Mutex::new(Instant::now()),
        }
    }
}

#[async_trait]
impl LlmClient for ScheduledLlmClient {
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        // Acquire a permit — blocks if max_concurrent calls are already in flight
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| LlmError::Other("inference scheduler semaphore closed".into()))?;

        // Optional rate smoothing
        if !self.min_delay.is_zero() {
            let mut last = self.last_call.lock().await;
            let elapsed = last.elapsed();
            if elapsed < self.min_delay {
                tokio::time::sleep(self.min_delay - elapsed).await;
            }
            *last = Instant::now();
        }

        tracing::debug!(
            agent_id = %request.agent_id,
            available_permits = self.semaphore.available_permits(),
            "inference permit acquired"
        );

        self.inner.complete(request).await
    }

    fn max_context_tokens(&self, model: &str) -> u32 {
        self.inner.max_context_tokens(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CompletionResponse, ContentBlock, LlmStopReason, Message};
    use aaos_core::{AgentId, TokenUsage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// Mock LlmClient that tracks concurrent calls.
    struct ConcurrencyTracker {
        active: StdArc<AtomicUsize>,
        max_seen: StdArc<AtomicUsize>,
        delay: Duration,
    }

    impl ConcurrencyTracker {
        fn new(delay: Duration) -> (Self, StdArc<AtomicUsize>, StdArc<AtomicUsize>) {
            let active = StdArc::new(AtomicUsize::new(0));
            let max_seen = StdArc::new(AtomicUsize::new(0));
            (
                Self {
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    delay,
                },
                active,
                max_seen,
            )
        }

        fn ok_response() -> CompletionResponse {
            CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }
        }
    }

    #[async_trait]
    impl LlmClient for ConcurrencyTracker {
        async fn complete(&self, _request: CompletionRequest) -> LlmResult<CompletionResponse> {
            let prev = self.active.fetch_add(1, Ordering::SeqCst);
            let current = prev + 1;
            // Update max_seen if this is the highest concurrent count
            self.max_seen.fetch_max(current, Ordering::SeqCst);

            tokio::time::sleep(self.delay).await;

            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Self::ok_response())
        }

        fn max_context_tokens(&self, _model: &str) -> u32 {
            128_000
        }
    }

    fn test_request() -> CompletionRequest {
        CompletionRequest {
            agent_id: AgentId::new(),
            model: "test".into(),
            system: "".into(),
            messages: vec![Message::User {
                content: "hi".into(),
            }],
            tools: vec![],
            max_tokens: 100,
        }
    }

    #[tokio::test]
    async fn semaphore_limits_concurrency() {
        let (tracker, _active, max_seen) =
            ConcurrencyTracker::new(Duration::from_millis(50));
        let client = Arc::new(ScheduledLlmClient::new(
            Arc::new(tracker),
            InferenceSchedulingConfig {
                max_concurrent: 2,
                min_delay_ms: 0,
            },
        ));

        // Spawn 6 concurrent requests with max_concurrent=2
        let mut handles = vec![];
        for _ in 0..6 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                c.complete(test_request()).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // At most 2 should have been active simultaneously
        assert!(max_seen.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn passthrough_max_context_tokens() {
        let (tracker, _, _) = ConcurrencyTracker::new(Duration::ZERO);
        let client = ScheduledLlmClient::new(
            Arc::new(tracker),
            InferenceSchedulingConfig::default(),
        );
        assert_eq!(client.max_context_tokens("anything"), 128_000);
    }

    #[tokio::test]
    async fn config_from_env_defaults() {
        // Clear env vars to get defaults
        std::env::remove_var("AAOS_MAX_CONCURRENT_INFERENCE");
        std::env::remove_var("AAOS_MIN_INFERENCE_DELAY_MS");
        let config = InferenceSchedulingConfig::from_env();
        assert_eq!(config.max_concurrent, 3);
        assert_eq!(config.min_delay_ms, 0);
    }

    #[tokio::test]
    async fn requests_complete_correctly() {
        let (tracker, _, _) = ConcurrencyTracker::new(Duration::ZERO);
        let client = ScheduledLlmClient::new(
            Arc::new(tracker),
            InferenceSchedulingConfig::default(),
        );
        let resp = client.complete(test_request()).await.unwrap();
        assert_eq!(resp.stop_reason, LlmStopReason::EndTurn);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "ok"));
    }
}
