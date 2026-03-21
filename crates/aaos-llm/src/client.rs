use async_trait::async_trait;

use crate::error::LlmResult;
use crate::types::{CompletionRequest, CompletionResponse};

/// Abstraction over LLM inference providers.
///
/// The daemon holds an `Arc<dyn LlmClient>` and passes it to `AgentExecutor`.
/// In tests, this is mocked with scripted responses.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse>;
}
