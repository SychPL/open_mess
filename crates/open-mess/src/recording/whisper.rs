//! Whisper's network: a copy of candle-transformers 0.11.0 `src/models/whisper/model.rs` (MIT OR
//! Apache-2.0) with two changes (spec 2026-10-28-local-transcription, 4.3). The decoder keeps the
//! keys and values of the tokens it has seen, so each step runs only the new token; the original
//! runs the whole sequence again, twice as slow on the CPU. The encoder checks between its layers
//! whether the transcription was cancelled, so a call that starts waits a fraction of a second
//! instead of a whole pass. Weight names and shapes are the original's.

use std::sync::atomic::AtomicBool;

use candle_core::{D, Device, IndexOp, Tensor};
use candle_nn::{Conv1d, Conv1dConfig, Embedding, LayerNorm, Linear, Module, VarBuilder, embedding, linear, linear_no_bias};
use candle_transformers::models::whisper::Config;

use super::stt::{SttError, check};

fn conv1d(inputs: usize, outputs: usize, kernel: usize, config: Conv1dConfig, vb: VarBuilder) -> Result<Conv1d, SttError> {
    Ok(Conv1d::new(vb.get((outputs, inputs, kernel), "weight")?, Some(vb.get(outputs, "bias")?), config))
}

fn layer_norm(size: usize, vb: VarBuilder) -> Result<LayerNorm, SttError> {
    Ok(LayerNorm::new(vb.get(size, "weight")?, vb.get(size, "bias")?, 1e-5))
}

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    out: Linear,
    heads: usize,
    /// Keys and values of the encoder's output (cross-attention), computed once per window.
    cross: Option<(Tensor, Tensor)>,
    /// Keys and values of the tokens seen so far (the decoder's self-attention).
    own: Option<(Tensor, Tensor)>,
}

impl Attention {
    fn load(state: usize, heads: usize, vb: VarBuilder) -> Result<Attention, SttError> {
        Ok(Attention {
            query: linear(state, state, vb.pp("q_proj"))?,
            value: linear(state, state, vb.pp("v_proj"))?,
            key: linear_no_bias(state, state, vb.pp("k_proj"))?,
            out: linear(state, state, vb.pp("out_proj"))?,
            heads,
            cross: None,
            own: None,
        })
    }

    /// `xa`: attend to the encoder's output instead of `x`. `keep`: add `x`'s keys and values to
    /// the ones kept, `flush` first dropping them (a new sequence or window).
    fn forward(&mut self, x: &Tensor, xa: Option<&Tensor>, mask: Option<&Tensor>, flush: bool, keep: bool) -> Result<Tensor, SttError> {
        let query = self.query.forward(x)?;
        let (key, value) = match xa {
            None => {
                let (mut key, mut value) = (self.key.forward(x)?, self.value.forward(x)?);
                if keep {
                    if flush {
                        self.own = None;
                    }
                    if let Some((kept_key, kept_value)) = &self.own {
                        key = Tensor::cat(&[kept_key, &key], 1)?;
                        value = Tensor::cat(&[kept_value, &value], 1)?;
                    }
                    self.own = Some((key.clone(), value.clone()));
                }
                (key, value)
            }
            Some(xa) => {
                if flush {
                    self.cross = None;
                }
                match &self.cross {
                    Some((key, value)) => (key.clone(), value.clone()),
                    None => {
                        let (key, value) = (self.key.forward(xa)?, self.value.forward(xa)?);
                        self.cross = Some((key.clone(), value.clone()));
                        (key, value)
                    }
                }
            }
        };
        let attended = self.attend(&query, &key, &value, mask)?;
        Ok(self.out.forward(&attended)?)
    }

    fn split_heads(&self, x: &Tensor) -> Result<Tensor, SttError> {
        let (batch, positions, state) = x.dims3()?;
        Ok(x.reshape((batch, positions, self.heads, state / self.heads))?.transpose(1, 2)?)
    }

    fn attend(&self, query: &Tensor, key: &Tensor, value: &Tensor, mask: Option<&Tensor>) -> Result<Tensor, SttError> {
        let (_, queries, state) = query.dims3()?;
        let keys = key.dim(1)?;
        let scale = ((state / self.heads) as f64).powf(-0.25);
        let query = (self.split_heads(query)? * scale)?;
        let key = (self.split_heads(key)?.transpose(2, 3)? * scale)?;
        let value = self.split_heads(value)?.contiguous()?;
        let mut weights = query.matmul(&key)?;
        if let Some(mask) = mask {
            // The new tokens come after the kept ones: their rows of the causal mask.
            let kept = keys.checked_sub(queries).ok_or_else(|| SttError::Model("fewer keys than queries".into()))?;
            weights = weights.broadcast_add(&mask.i((kept..kept + queries, 0..keys))?)?;
        }
        Ok(candle_nn::ops::softmax_last_dim(&weights)?.matmul(&value)?.transpose(1, 2)?.flatten_from(2)?)
    }
}

struct Block {
    attention: Attention,
    attention_ln: LayerNorm,
    cross: Option<(Attention, LayerNorm)>,
    fc1: Linear,
    fc2: Linear,
    mlp_ln: LayerNorm,
}

impl Block {
    fn load(state: usize, heads: usize, cross: bool, vb: VarBuilder) -> Result<Block, SttError> {
        let cross = if cross {
            Some((Attention::load(state, heads, vb.pp("encoder_attn"))?, layer_norm(state, vb.pp("encoder_attn_layer_norm"))?))
        } else {
            None
        };
        Ok(Block {
            attention: Attention::load(state, heads, vb.pp("self_attn"))?,
            attention_ln: layer_norm(state, vb.pp("self_attn_layer_norm"))?,
            cross,
            fc1: linear(state, state * 4, vb.pp("fc1"))?,
            fc2: linear(state * 4, state, vb.pp("fc2"))?,
            mlp_ln: layer_norm(state, vb.pp("final_layer_norm"))?,
        })
    }

    fn forward(&mut self, x: &Tensor, xa: Option<&Tensor>, mask: Option<&Tensor>, flush: bool) -> Result<Tensor, SttError> {
        // Only a decoder block (the one with cross-attention) keeps its keys and values.
        let keep = self.cross.is_some();
        let mut x = (x + self.attention.forward(&self.attention_ln.forward(x)?, None, mask, flush, keep)?)?;
        if let Some((attention, ln)) = &mut self.cross {
            x = (&x + attention.forward(&ln.forward(&x)?, xa, None, flush, false)?)?;
        }
        let mlp = self.fc2.forward(&self.fc1.forward(&self.mlp_ln.forward(&x)?)?.gelu()?)?;
        Ok((x + mlp)?)
    }
}

fn sinusoids(length: usize, channels: usize, device: &Device) -> Result<Tensor, SttError> {
    let half = channels / 2;
    let increment = 10000f32.ln() / half.saturating_sub(1).max(1) as f32;
    let inverse: Vec<f32> = (0..half).map(|i| (i as f32 * -increment).exp()).collect();
    let inverse = Tensor::new(inverse.as_slice(), device)?.unsqueeze(0)?;
    let positions = Tensor::arange(0, length as u32, device)?.to_dtype(candle_core::DType::F32)?.unsqueeze(1)?;
    let scaled = (positions.broadcast_as((length, half))? * inverse.broadcast_as((length, half))?)?;
    Ok(Tensor::cat(&[scaled.sin()?, scaled.cos()?], 1)?)
}

pub struct Encoder {
    conv1: Conv1d,
    conv2: Conv1d,
    positions: Tensor,
    blocks: Vec<Block>,
    ln_post: LayerNorm,
}

impl Encoder {
    fn load(vb: VarBuilder, config: &Config) -> Result<Encoder, SttError> {
        let state = config.d_model;
        let conv = |stride| Conv1dConfig { padding: 1, stride, groups: 1, dilation: 1, cudnn_fwd_algo: None };
        Ok(Encoder {
            conv1: conv1d(config.num_mel_bins, state, 3, conv(1), vb.pp("conv1"))?,
            conv2: conv1d(state, state, 3, conv(2), vb.pp("conv2"))?,
            positions: sinusoids(config.max_source_positions, state, vb.device())?,
            blocks: (0..config.encoder_layers)
                .map(|i| Block::load(state, config.encoder_attention_heads, false, vb.pp(format!("layers.{i}"))))
                .collect::<Result<_, _>>()?,
            ln_post: layer_norm(state, vb.pp("layer_norm"))?,
        })
    }

    /// The encoder's output for one window's log-mel; stops with [`SttError::Cancelled`] between
    /// layers once `cancel` is set.
    pub fn forward(&mut self, mel: &Tensor, cancel: &AtomicBool) -> Result<Tensor, SttError> {
        let x = self.conv2.forward(&self.conv1.forward(mel)?.gelu()?)?.gelu()?.transpose(1, 2)?;
        let positions = x.dim(1)?;
        let mut x = x.broadcast_add(&self.positions.narrow(0, 0, positions)?)?;
        for block in &mut self.blocks {
            check(cancel)?;
            x = block.forward(&x, None, None, true)?;
        }
        Ok(self.ln_post.forward(&x)?)
    }
}

pub struct Decoder {
    token_embedding: Embedding,
    positions: Tensor,
    blocks: Vec<Block>,
    ln: LayerNorm,
    mask: Tensor,
    /// Tokens seen since the last flush.
    seen: usize,
}

impl Decoder {
    fn load(vb: VarBuilder, config: &Config) -> Result<Decoder, SttError> {
        let state = config.d_model;
        let context = config.max_target_positions;
        let mask: Vec<f32> = (0..context).flat_map(|i| (0..context).map(move |j| if j > i { f32::NEG_INFINITY } else { 0.0 })).collect();
        Ok(Decoder {
            token_embedding: embedding(config.vocab_size, state, vb.pp("embed_tokens"))?,
            positions: vb.get((context, state), "embed_positions.weight")?,
            blocks: (0..config.decoder_layers)
                .map(|i| Block::load(state, config.decoder_attention_heads, true, vb.pp(format!("layers.{i}"))))
                .collect::<Result<_, _>>()?,
            ln: layer_norm(state, vb.pp("layer_norm"))?,
            mask: Tensor::from_vec(mask, (context, context), vb.device())?,
            seen: 0,
        })
    }

    /// Runs `tokens`, the ones not given since the last flush, against the encoder's output `xa`;
    /// `flush` starts a new sequence. One row of output per token given.
    pub fn forward(&mut self, tokens: &Tensor, xa: &Tensor, flush: bool) -> Result<Tensor, SttError> {
        if flush {
            self.seen = 0;
        }
        let count = tokens.dim(D::Minus1)?;
        let mut x = self.token_embedding.forward(tokens)?.broadcast_add(&self.positions.narrow(0, self.seen, count)?)?;
        for block in &mut self.blocks {
            x = block.forward(&x, Some(xa), Some(&self.mask), flush)?;
        }
        self.seen += count;
        Ok(self.ln.forward(&x)?)
    }

    pub fn final_linear(&self, x: &Tensor) -> Result<Tensor, SttError> {
        let weights = self.token_embedding.embeddings().broadcast_left(x.dim(0)?)?;
        Ok(x.matmul(&weights.t()?)?)
    }
}

pub struct Whisper {
    pub encoder: Encoder,
    pub decoder: Decoder,
    pub config: Config,
}

impl Whisper {
    pub fn load(vb: &VarBuilder, config: Config) -> Result<Whisper, SttError> {
        let heads = [config.encoder_attention_heads, config.decoder_attention_heads];
        if config.d_model < 4 || heads.iter().any(|&heads| heads == 0 || !config.d_model.is_multiple_of(heads)) {
            return Err(SttError::Model("the model's attention heads do not fit its width".into()));
        }
        Ok(Whisper { encoder: Encoder::load(vb.pp("model.encoder"), &config)?, decoder: Decoder::load(vb.pp("model.decoder"), &config)?, config })
    }
}

#[cfg(test)]
mod tests {
    use candle_core::{DType, Shape};
    use candle_nn::var_builder::SimpleBackend;

    use super::*;

    /// Random weights for any name, so the network computes something other than zeros.
    struct Random;

    impl SimpleBackend for Random {
        fn get(&self, shape: Shape, _: &str, _: candle_nn::Init, dtype: DType, device: &Device) -> candle_core::Result<Tensor> {
            Tensor::randn(0f32, 0.5, shape, device)?.to_dtype(dtype)
        }

        fn get_unchecked(&self, name: &str, _: DType, _: &Device) -> candle_core::Result<Tensor> {
            candle_core::bail!("no shape for {name}")
        }

        fn contains_tensor(&self, _: &str) -> bool {
            true
        }
    }

    fn small() -> Whisper {
        let config = Config {
            num_mel_bins: 8,
            max_source_positions: 10,
            d_model: 8,
            encoder_attention_heads: 2,
            encoder_layers: 2,
            vocab_size: 16,
            max_target_positions: 12,
            decoder_attention_heads: 2,
            decoder_layers: 2,
            suppress_tokens: Vec::new(),
        };
        Whisper::load(&VarBuilder::from_backend(Box::new(Random), DType::F32, Device::Cpu), config).unwrap()
    }

    fn row(x: &Tensor, at: usize) -> Vec<f32> {
        x.i((0, at)).unwrap().to_vec1().unwrap()
    }

    #[test]
    fn the_kept_keys_give_the_same_output_as_the_whole_sequence() {
        let mut whisper = small();
        let mel = Tensor::randn(0f32, 1.0, (1, 8, 20), &Device::Cpu).unwrap();
        let xa = whisper.encoder.forward(&mel, &AtomicBool::new(false)).unwrap();
        let tokens: [u32; 6] = [1, 5, 3, 9, 2, 7];
        let ids = |part: &[u32]| Tensor::new(part, &Device::Cpu).unwrap().unsqueeze(0).unwrap();
        // The original's way: the whole sequence at every step.
        let whole: Vec<Vec<f32>> =
            (3..=tokens.len()).map(|end| row(&whisper.decoder.forward(&ids(&tokens[..end]), &xa, true).unwrap(), end - 1)).collect();
        // This copy's way: the prompt, then one token at a time.
        let mut kept = vec![row(&whisper.decoder.forward(&ids(&tokens[..3]), &xa, true).unwrap(), 2)];
        for at in 3..tokens.len() {
            kept.push(row(&whisper.decoder.forward(&ids(&tokens[at..=at]), &xa, false).unwrap(), 0));
        }
        for (whole, kept) in whole.iter().zip(&kept) {
            assert!(whole.iter().zip(kept).all(|(a, b)| (a - b).abs() < 1e-4), "{whole:?} != {kept:?}");
        }
        // Past the decoder's positions: an error, not a panic.
        assert!(whisper.decoder.forward(&ids(&[1; 7]), &xa, false).is_err());
    }

    #[test]
    fn the_encoder_stops_between_layers() {
        let mut whisper = small();
        let mel = Tensor::zeros((1, 8, 20), DType::F32, &Device::Cpu).unwrap();
        assert!(matches!(whisper.encoder.forward(&mel, &AtomicBool::new(true)), Err(SttError::Cancelled)));
    }
}
