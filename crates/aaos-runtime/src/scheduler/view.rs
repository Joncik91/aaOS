//! `SchedulerView` — per-subtask wrapper that routes every `complete()`
//! call through the reasoning scheduler before delegating to the real
//! client, then records wall-clock elapsed in the latency tracker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use aaos_llm::{CompletionRequest, CompletionResponse, LlmClient, LlmResult};

use super::{LatencyTracker, ReasoningScheduler};

pub struct SchedulerView {
    inner: Arc<dyn LlmClient>,
    scheduler: Arc<ReasoningScheduler>,
    latency: Arc<dyn LatencyTracker>,
    subtask_id: String,
    priority: u8,
    deadline: Option<Instant>,
}

impl SchedulerView {
    pub fn new(
        inner: Arc<dyn LlmClient>,
        scheduler: Arc<ReasoningScheduler>,
        latency: Arc<dyn LatencyTracker>,
        subtask_id: String,
        priority: u8,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            inner,
            scheduler,
            latency,
            subtask_id,
            priority,
            deadline,
        }
    }
}

#[async_trait]
impl LlmClient for SchedulerView {
    async fn complete(&self, req: CompletionRequest) -> LlmResult<CompletionResponse> {
        let _permit = self
            .scheduler
            .acquire_slot(self.subtask_id.clone(), self.priority, self.deadline)
            .await;
        let start = Instant::now();
        let result = self.inner.complete(req).await;
        self.latency.record(&self.subtask_id, start.elapsed());
        result
    }

    fn max_context_tokens(&self, model: &str) -> u32 {
        self.inner.max_context_tokens(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::SubtaskWallClockTracker;

    use aaos_core::{AgentId, TokenUsage};
    use aaos_llm::{ContentBlock, LlmStopReason, Message};
    use async_trait::async_trait;

    struct SlowClient {
        delay: Duration,
    }
    #[async_trait]
    impl LlmClient for SlowClient {
        async fn complete(&self, _r: CompletionRequest) -> LlmResult<CompletionResponse> {
            tokio::time::sleep(self.delay).await;
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: LlmStopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
        fn max_context_tokens(&self, _m: &str) -> u32 {
            200_000
        }
    }

    #[tokio::test]
    async fn records_elapsed_into_latency_tracker() {
        let scheduler = ReasoningScheduler::new(4);
        let latency: Arc<dyn LatencyTracker> = Arc::new(SubtaskWallClockTracker::new());
        let inner: Arc<dyn LlmClient> = Arc::new(SlowClient {
            delay: Duration::from_millis(100),
        });
        let view = SchedulerView::new(inner, scheduler, latency.clone(), "sub-1".into(), 128, None);
        let _ = view
            .complete(CompletionRequest {
                agent_id: AgentId::new(),
                model: "test".into(),
                system: String::new(),
                messages: vec![Message::User {
                    content: "hi".into(),
                }],
                tools: vec![],
                max_tokens: 100,
            })
            .await
            .unwrap();
        assert!(
            latency.wall_clock_elapsed("sub-1") >= Duration::from_millis(95),
            "expected at least ~100ms recorded; got {:?}",
            latency.wall_clock_elapsed("sub-1")
        );
    }
}
