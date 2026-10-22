use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use model2vec_rs::model::StaticModel;

/// Folder under herdr's state directory that holds the model files (`config.json`,
/// `tokenizer.json`, `model.safetensors`) of Model2Vec's `minishlab/potion-retrieval-32M`:
/// a static embedding model, so a text is the mean of its tokens' vectors and embedding a title
/// or a keyword list takes microseconds on one CPU thread.
const MODEL_DIR_NAME: &str = "potion-retrieval-32M";

pub(super) fn model_dir() -> PathBuf {
    crate::config::state_dir()
        .join("models")
        .join(MODEL_DIR_NAME)
}

/// Turns text into a vector; texts about similar things get vectors with a high cosine.
pub(crate) trait Embedder: Send + Sync {
    /// `None` when the text has nothing the model knows.
    fn embed(&self, text: &str) -> Option<Vec<f32>>;
}

pub(super) struct StaticEmbedder {
    model: StaticModel,
}

impl StaticEmbedder {
    pub(super) fn load(dir: &Path) -> Result<Self, String> {
        StaticModel::from_pretrained(dir, None, None, None)
            .map(|model| Self { model })
            .map_err(|error| format!("{error:#}"))
    }
}

impl Embedder for StaticEmbedder {
    fn embed(&self, text: &str) -> Option<Vec<f32>> {
        // The model crate panics when tokenization fails; a chat title must not take the window down.
        let vector = catch_unwind(AssertUnwindSafe(|| self.model.encode_single(text))).ok()?;
        vector.iter().any(|value| *value != 0.0).then_some(vector)
    }
}
