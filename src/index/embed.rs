//! Local sentence embeddings via Hugging Face Candle (BERT + mean pooling).
//! Loaded lazily so non-index commands never pay the startup cost, and fully
//! optional: if the model can't load (offline first run, disabled via env),
//! the vault still works on FTS5 + the wikilink graph.

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{api::sync::Api, Repo, RepoType};
use std::sync::{Mutex, OnceLock};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub const DEFAULT_MODEL: &str = "TaylorAI/bge-micro-v2";
pub const EMBED_DIM: usize = 384;
const MAX_INPUT_CHARS: usize = 2000;
const MODEL_MAX_TOKENS: usize = 512;

struct EmbedderState {
    init_tried: bool,
    model: Option<LoadedModel>,
}

struct LoadedModel {
    bert: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

static STATE: OnceLock<Mutex<EmbedderState>> = OnceLock::new();

fn state() -> &'static Mutex<EmbedderState> {
    STATE.get_or_init(|| {
        Mutex::new(EmbedderState {
            init_tried: false,
            model: None,
        })
    })
}

pub fn embeddings_disabled() -> bool {
    std::env::var("VAULT_AXI_NO_EMBED").as_deref() == Ok("1")
}

pub fn model_name() -> String {
    std::env::var("VAULT_AXI_EMBED_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn load_model() -> anyhow::Result<LoadedModel> {
    let api = Api::new()?;
    let repo = api.repo(Repo::new(model_name(), RepoType::Model));
    let config_path = repo.get("config.json")?;
    let tokenizer_path = repo.get("tokenizer.json")?;
    let weights_path = repo
        .get("model.safetensors")
        .or_else(|_| repo.get("onnx/model.safetensors"))?;

    let config_str = std::fs::read_to_string(config_path)?;
    let config: BertConfig = serde_json::from_str(&config_str)?;
    let mut tokenizer = Tokenizer::from_file(tokenizer_path)
        .map_err(|e| anyhow::anyhow!("Tokenizer load error: {e}"))?;

    let _ = tokenizer.with_truncation(Some(TruncationParams {
        max_length: MODEL_MAX_TOKENS,
        ..Default::default()
    }));
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        ..Default::default()
    }));

    let device = Device::Cpu;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)? };
    let bert = BertModel::load(vb, &config)?;

    Ok(LoadedModel {
        bert,
        tokenizer,
        device,
    })
}

/// Attempt to load the model. Returns false if embeddings are unavailable.
pub fn init_embedder() -> bool {
    if embeddings_disabled() {
        return false;
    }
    let mut guard = state().lock().unwrap();
    if guard.init_tried {
        return guard.model.is_some();
    }
    guard.init_tried = true;
    match load_model() {
        Ok(loaded) => {
            guard.model = Some(loaded);
            true
        }
        Err(err) => {
            if std::env::var("VAULT_AXI_DEBUG").as_deref() == Ok("1") {
                eprintln!("[vault-axi] embedder init failed: {err:?}");
            }
            guard.model = None;
            false
        }
    }
}

pub fn is_available() -> bool {
    if embeddings_disabled() {
        return false;
    }
    let guard = state().lock().unwrap();
    guard.model.is_some()
}

/// Embed a batch of texts into normalized f32 vectors, or None if unavailable.
pub fn embed(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if embeddings_disabled() {
        return None;
    }
    if !is_available() && !init_embedder() {
        return None;
    }
    if texts.is_empty() {
        return Some(Vec::new());
    }

    let guard = state().lock().unwrap();
    let loaded = guard.model.as_ref()?;

    let clipped: Vec<String> = texts
        .iter()
        .map(|t| t.chars().take(MAX_INPUT_CHARS).collect())
        .collect();

    let encodings = loaded.tokenizer.encode_batch(clipped, true).ok()?;
    let batch_size = encodings.len();
    let seq_len = encodings.first()?.get_ids().len();

    let mut ids_vec = Vec::with_capacity(batch_size * seq_len);
    let mut mask_vec = Vec::with_capacity(batch_size * seq_len);
    let mut type_ids_vec = Vec::with_capacity(batch_size * seq_len);

    for enc in &encodings {
        ids_vec.extend_from_slice(enc.get_ids());
        mask_vec.extend_from_slice(enc.get_attention_mask());
        type_ids_vec.extend_from_slice(enc.get_type_ids());
    }

    let token_ids = Tensor::from_vec(ids_vec, (batch_size, seq_len), &loaded.device).ok()?;
    let attention_mask_u32 =
        Tensor::from_vec(mask_vec, (batch_size, seq_len), &loaded.device).ok()?;
    let token_type_ids =
        Tensor::from_vec(type_ids_vec, (batch_size, seq_len), &loaded.device).ok()?;

    let embeddings = loaded
        .bert
        .forward(&token_ids, &token_type_ids, Some(&attention_mask_u32))
        .ok()?;

    let mask_f32 = attention_mask_u32
        .to_dtype(DType::F32)
        .ok()?
        .unsqueeze(2)
        .ok()?
        .broadcast_as(embeddings.shape())
        .ok()?;
    let masked = (embeddings * &mask_f32).ok()?;
    let sum_embeddings = masked.sum(1).ok()?;
    let sum_mask = mask_f32.sum(1).ok()?.clamp(1e-9, f64::MAX).ok()?;
    let pooled = sum_embeddings.broadcast_div(&sum_mask).ok()?;

    let raw_rows: Vec<Vec<f32>> = pooled.to_vec2().ok()?;
    let normalized = raw_rows
        .into_iter()
        .map(|mut row| {
            let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in &mut row {
                    *x /= norm;
                }
            }
            row
        })
        .collect();

    Some(normalized)
}

pub fn to_blob(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for &v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn from_blob(buf: &[u8]) -> Vec<f32> {
    buf.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity for already-normalized vectors reduces to a dot product.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}
