use async_trait::async_trait;
use crate::store::MemoryError;

/// Trait for generating text embeddings.
#[async_trait]
pub trait EmbeddingSource: Send + Sync {
    /// Generate an embedding vector for the given text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError>;

    /// The dimensionality of vectors produced by this source.
    fn dimensions(&self) -> usize;

    /// The model name (stored alongside embeddings for mismatch detection).
    fn model_name(&self) -> &str;
}

/// Mock embedding source for testing. Returns deterministic vectors.
pub struct MockEmbeddingSource {
    dims: usize,
    model: String,
}

impl MockEmbeddingSource {
    pub fn new(dims: usize) -> Self {
        Self { dims, model: "mock-embed".into() }
    }

    pub fn with_model(dims: usize, model: &str) -> Self {
        Self { dims, model: model.into() }
    }
}

#[async_trait]
impl EmbeddingSource for MockEmbeddingSource {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        // Generate a deterministic vector from the text hash.
        // Different texts produce different vectors, same text produces same vector.
        let mut embedding = vec![0.0f32; self.dims];
        let bytes = text.as_bytes();
        for (i, chunk) in bytes.chunks(4).enumerate() {
            let idx = i % self.dims;
            let val: f32 = chunk.iter().enumerate().fold(0.0, |acc, (j, &b)| {
                acc + (b as f32) / (256.0 * (j + 1) as f32)
            });
            embedding[idx] += val;
        }
        // Normalize to unit vector.
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for v in &mut embedding {
                *v /= magnitude;
            }
        }
        Ok(embedding)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// Embedding source using Ollama's OpenAI-compatible /v1/embeddings endpoint.
pub struct OllamaEmbeddingSource {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dims: usize,
}

impl OllamaEmbeddingSource {
    /// Create a new OllamaEmbeddingSource.
    /// - base_url: e.g., "http://localhost:11434"
    /// - model: e.g., "nomic-embed-text"
    /// - dims: expected embedding dimensions (768 for nomic-embed-text)
    pub fn new(base_url: &str, model: &str, dims: usize) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dims,
        }
    }
}

#[async_trait]
impl EmbeddingSource for OllamaEmbeddingSource {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let response = self.client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Embedding(format!("HTTP error: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Embedding(
                format!("Ollama API error {status}: {body}")
            ));
        }

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| MemoryError::Embedding(format!("JSON parse error: {e}")))?;

        // OpenAI-compatible response: {"data": [{"embedding": [...]}]}
        let embedding = json["data"][0]["embedding"]
            .as_array()
            .ok_or_else(|| MemoryError::Embedding("missing embedding in response".into()))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect::<Vec<f32>>();

        if embedding.len() != self.dims {
            return Err(MemoryError::DimensionMismatch {
                expected: self.dims,
                actual: embedding.len(),
            });
        }

        Ok(embedding)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_embedding_deterministic() {
        let source = MockEmbeddingSource::new(64);
        let v1 = source.embed("hello world").await.unwrap();
        let v2 = source.embed("hello world").await.unwrap();
        assert_eq!(v1, v2);
        assert_eq!(v1.len(), 64);
    }

    #[tokio::test]
    async fn mock_embedding_different_texts_differ() {
        let source = MockEmbeddingSource::new(64);
        let v1 = source.embed("hello world").await.unwrap();
        let v2 = source.embed("goodbye world").await.unwrap();
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn mock_embedding_normalized() {
        let source = MockEmbeddingSource::new(64);
        let v = source.embed("test text").await.unwrap();
        let magnitude: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.01, "vector should be unit normalized, got {magnitude}");
    }

    #[test]
    fn mock_dimensions_and_model() {
        let source = MockEmbeddingSource::new(128);
        assert_eq!(source.dimensions(), 128);
        assert_eq!(source.model_name(), "mock-embed");

        let source2 = MockEmbeddingSource::with_model(64, "custom");
        assert_eq!(source2.model_name(), "custom");
    }

    #[test]
    fn ollama_source_construction() {
        let source = OllamaEmbeddingSource::new("http://localhost:11434", "nomic-embed-text", 768);
        assert_eq!(source.dimensions(), 768);
        assert_eq!(source.model_name(), "nomic-embed-text");
    }
}
