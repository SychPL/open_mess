//! Speech to text with Whisper on candle, on the CPU (spec 2026-10-28-local-transcription, 4.3).
//! Each track is cut into 30 s windows. A silent window is skipped, since Whisper invents text
//! over silence; the others go through the encoder and a decoder with timestamps that falls back
//! to higher temperatures, as Whisper does. Everything the model writes is untrusted: lengths are
//! capped, and none of it reaches a path or a log.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::{Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self as m, Config, audio};
use serde::Deserialize;

use super::model::{self, Revision};
use super::whisper::Whisper;
use super::{Cue, MAX_TRANSCRIPT, Speaker, TRACKS, Transcript, wav};

/// The mel filters of Whisper large-v3 (128 bands of 201 FFT bins, f32 little-endian), the file
/// of the whisper example in candle 0.11.0 (MIT OR Apache-2.0).
const MEL_FILTERS: &[u8] = include_bytes!("melfilters128.bytes");
const MEL_BINS: usize = 128;
/// A window whose RMS (of 32768) is below this is silence and skipped.
/// ponytail: a guess until step 0 measures real calls; a per-recording noise floor if it misfires.
const SILENCE_RMS: f64 = 60.0;
/// The first timestamp may be at most this many 20 ms steps into a window (Whisper's 1 s).
const MAX_INITIAL_TIMESTAMP: u32 = 50;
/// Characters of one cue, and cues of one transcript (4.3).
const MAX_CUE_TEXT: usize = 4000;
const MAX_CUES: usize = 100_000;
/// Marks text cut short.
const CUT: &str = " [...]";

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("transcription stopped")]
    Cancelled,
    #[error("the transcription crashed")]
    Panicked,
    #[error("the model does not fit: {0}")]
    Model(String),
    #[error("model: {0}")]
    Candle(#[from] candle_core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Runs a transcription on its own thread, with a pool of half the cores so the rest of the
/// computer keeps working (4.3). A panic in it (candle, the model) ends as
/// [`SttError::Panicked`], never in the daemon.
pub fn isolated<T: Send + 'static>(job: impl FnOnce() -> Result<T, SttError> + Send + 'static) -> Result<T, SttError> {
    let threads = std::thread::available_parallelism().map_or(1, |cores| (cores.get() / 2).max(1));
    let thread = std::thread::Builder::new().name("transcription".into()).spawn(move || {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().map_err(|err| SttError::Model(err.to_string()))?;
        pool.install(job)
    })?;
    thread.join().unwrap_or(Err(SttError::Panicked))
}

pub(super) fn check(cancel: &AtomicBool) -> Result<(), SttError> {
    if cancel.load(Ordering::SeqCst) { Err(SttError::Cancelled) } else { Ok(()) }
}

#[derive(Deserialize)]
struct TokenizerFile {
    model: BpeModel,
    added_tokens: Vec<AddedToken>,
}

#[derive(Deserialize)]
struct BpeModel {
    vocab: HashMap<String, u32>,
}

#[derive(Deserialize)]
struct AddedToken {
    id: u32,
    content: String,
}

/// The bytes GPT-2's byte-level BPE writes as themselves: the printable Latin-1 ones. The others
/// are written as the characters from U+0100 on, in byte order.
fn printable(byte: u8) -> bool {
    matches!(byte, 33..=126 | 161..=172 | 174..=255)
}

/// What the decoder needs of `tokenizer.json`.
#[derive(Debug)]
pub struct Vocab {
    /// Each text token's bytes, by id; the ids from `eot` on are special.
    text: Vec<Vec<u8>>,
    sot: u32,
    eot: u32,
    transcribe: u32,
    no_speech: u32,
    no_timestamps: u32,
    /// `<|pl|>` and the others, by code: the ids between `<|startoftranscript|>` and
    /// `<|translate|>`.
    languages: Vec<(String, u32)>,
}

impl Vocab {
    pub fn parse(json: &[u8]) -> Result<Vocab, SttError> {
        let file: TokenizerFile = serde_json::from_slice(json).map_err(|err| SttError::Model(format!("tokenizer.json: {err}")))?;
        let special: HashMap<&str, u32> = file.added_tokens.iter().map(|token| (token.content.as_str(), token.id)).collect();
        let id = |name: &str| special.get(name).copied().ok_or_else(|| SttError::Model(format!("no token {name}")));
        let (sot, eot, translate) = (id(m::SOT_TOKEN)?, id(m::EOT_TOKEN)?, id(m::TRANSLATE_TOKEN)?);
        let no_speech = m::NO_SPEECH_TOKENS.iter().find_map(|name| id(name).ok()).ok_or_else(|| SttError::Model("no no-speech token".into()))?;
        let no_timestamps = id(m::NO_TIMESTAMPS_TOKEN)?;
        if eot as usize > 1_000_000 || !(eot < sot && sot < translate && translate < no_timestamps) {
            return Err(SttError::Model("special tokens out of order".into()));
        }
        let shifted: Vec<u8> = (0..=255u8).filter(|byte| !printable(*byte)).collect();
        let byte_of = |c: char| -> Option<u8> {
            let code = u32::from(c);
            match u8::try_from(code) {
                Ok(byte) => printable(byte).then_some(byte),
                Err(_) => shifted.get(code.checked_sub(256)? as usize).copied(),
            }
        };
        let mut text = vec![Vec::new(); eot as usize];
        for (token, id) in &file.model.vocab {
            if let Some(slot) = text.get_mut(*id as usize) {
                *slot = token.chars().filter_map(byte_of).collect();
            }
        }
        let mut languages: Vec<(String, u32)> = file
            .added_tokens
            .iter()
            .filter(|token| token.id > sot && token.id < translate)
            .filter_map(|token| Some((token.content.strip_prefix("<|")?.strip_suffix("|>")?.to_string(), token.id)))
            .collect();
        languages.sort_by_key(|(_, id)| *id);
        Ok(Vocab { text, sot, eot, transcribe: id(m::TRANSCRIBE_TOKEN)?, no_speech, no_timestamps, languages })
    }

    /// The text of the text tokens among `tokens`; bytes that are not UTF-8 become U+FFFD.
    pub fn decode(&self, tokens: &[u32]) -> String {
        let bytes: Vec<u8> = tokens.iter().filter_map(|token| self.text.get(*token as usize)).flatten().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn language(&self, code: &str) -> Option<u32> {
        self.languages.iter().find(|(known, _)| known == code).map(|(_, id)| *id)
    }

    fn code(&self, id: u32) -> Option<String> {
        self.languages.iter().find(|(_, known)| *known == id).map(|(code, _)| code.clone())
    }

    fn timestamp_begin(&self) -> u32 {
        self.no_timestamps + 1
    }
}

fn log_softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if max == f32::NEG_INFINITY {
        return logits.to_vec();
    }
    let lse = max + logits.iter().map(|l| (l - max).exp()).sum::<f32>().ln();
    logits.iter().map(|l| l - lse).collect()
}

/// The first of the largest logits.
fn argmax(logits: &[f32]) -> u32 {
    let mut best = 0;
    for (i, logit) in logits.iter().enumerate() {
        if logits.get(best).is_none_or(|top| logit > top) {
            best = i;
        }
    }
    best as u32
}

/// A token drawn with the probabilities of `logits / temperature`.
fn sample(logits: &[f32], temperature: f64) -> Result<u32, SttError> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let weights: Vec<f64> = logits.iter().map(|l| f64::from(l - max) / temperature).map(f64::exp).collect();
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|err| SttError::Model(err.to_string()))?;
    let mut target = (u64::from_le_bytes(random) >> 11) as f64 / (1u64 << 53) as f64 * weights.iter().sum::<f64>();
    for (i, weight) in weights.iter().enumerate() {
        if target < *weight {
            return Ok(i as u32);
        }
        target -= weight;
    }
    Ok(argmax(logits))
}

/// Whisper's rules for timestamps (openai/whisper, `ApplyTimestampRules`), on the logits of the
/// next token after `sampled` (the tokens after the prompt). Also keeps out every special token
/// but `eot`.
fn timestamp_rules(vocab: &Vocab, suppress: &[u32], logits: &mut [f32], sampled: &[u32]) {
    let begin = vocab.timestamp_begin();
    let keep = |logits: &mut [f32], keep: &dyn Fn(u32) -> bool| {
        for (i, logit) in logits.iter_mut().enumerate() {
            if !keep(i as u32) {
                *logit = f32::NEG_INFINITY;
            }
        }
    };
    keep(logits, &|i| (i <= vocab.eot || i >= begin) && !suppress.contains(&i));
    let is_timestamp = |token: Option<&u32>| token.is_some_and(|token| *token >= begin);
    let last = is_timestamp(sampled.last());
    let penultimate = sampled.len() < 2 || is_timestamp(sampled.len().checked_sub(2).and_then(|i| sampled.get(i)));
    if last {
        if penultimate {
            // A pair just closed: text or the end must come.
            keep(logits, &|i| i < begin);
        } else {
            // An open timestamp: another timestamp or the end, no text.
            keep(logits, &|i| i >= vocab.eot);
        }
    }
    if let Some(latest) = sampled.iter().rev().find(|token| **token >= begin) {
        // Timestamps never go back.
        let lowest = if last && !penultimate { *latest } else { latest + 1 };
        keep(logits, &|i| i < begin || i >= lowest);
    }
    if sampled.is_empty() {
        keep(logits, &|i| i >= begin && i <= begin + MAX_INITIAL_TIMESTAMP);
    }
    // When the timestamps together are likelier than any text token, a timestamp it is.
    let logprobs = log_softmax(logits);
    let (text, stamps) = logprobs.split_at((begin as usize).min(logprobs.len()));
    let max_stamp = stamps.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let stamp = if max_stamp == f32::NEG_INFINITY { max_stamp } else { max_stamp + stamps.iter().map(|l| (l - max_stamp).exp()).sum::<f32>().ln() };
    if stamp > text.iter().copied().fold(f32::NEG_INFINITY, f32::max) {
        keep(logits, &|i| i >= begin);
    }
}

/// How much `text` shrinks when compressed: Whisper retries a window whose text loops.
fn compression_ratio(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    text.len() as f64 / miniz_oxide::deflate::compress_to_vec(text.as_bytes(), 6).len().max(1) as f64
}

fn silent(samples: &[i16]) -> bool {
    let energy: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    samples.is_empty() || (energy / samples.len() as f64).sqrt() < SILENCE_RMS
}

struct Decoding {
    tokens: Vec<u32>,
    avg_logprob: f64,
    no_speech_prob: f64,
    compression_ratio: f64,
    /// The decoder hit Whisper's token limit before the end of the window's text.
    cut: bool,
}

/// A window's pieces of speech from its sampled tokens: start and end in milliseconds from the
/// window's start (the end of the last piece unknown, `u64::MAX`), and text. A decoding cut at the
/// token limit marks its last piece.
fn pieces(vocab: &Vocab, tokens: &[u32], cut: bool) -> Vec<(u64, u64, String)> {
    let begin = vocab.timestamp_begin();
    let mut pieces = Vec::new();
    let (mut start, mut text) = (0u64, Vec::new());
    for token in tokens {
        if *token >= begin {
            let at = u64::from(token - begin) * 20;
            if !text.is_empty() {
                pieces.push((start, at, vocab.decode(&text)));
                text.clear();
            }
            start = at;
        } else if *token < vocab.eot {
            text.push(*token);
        }
    }
    if !text.is_empty() {
        pieces.push((start, u64::MAX, vocab.decode(&text)));
    }
    let mut pieces: Vec<_> =
        pieces.into_iter().map(|(start, end, text)| (start, end, text.trim().to_string())).filter(|(_, _, text)| !text.is_empty()).collect();
    if cut && let Some((_, _, text)) = pieces.last_mut() {
        text.push_str(CUT);
    }
    pieces
}

/// A loaded model, ready to transcribe.
pub struct Model {
    whisper: Whisper,
    vocab: Vocab,
    filters: Vec<f32>,
    device: Device,
}

impl Model {
    /// Loads a revision's files from `<local>\models`, each checked against its sum. The weights
    /// are read whole and handed over without mapping the file, which candle offers only as
    /// `unsafe` (AGENTS.md).
    pub fn load(local: &Path, revision: &Revision) -> Result<Model, SttError> {
        let dir = model::folder(local, revision);
        let [config, tokenizer, weights] = &revision.files;
        let config: Config =
            serde_json::from_slice(&model::read_verified(&dir, config)?).map_err(|err| SttError::Model(format!("config.json: {err}")))?;
        let vocab = Vocab::parse(&model::read_verified(&dir, tokenizer)?)?;
        let device = Device::Cpu;
        let vb = VarBuilder::from_buffered_safetensors(model::read_verified(&dir, weights)?, m::DTYPE, &device)?;
        Model::new(Whisper::load(&vb, config)?, vocab, device)
    }

    fn new(whisper: Whisper, vocab: Vocab, device: Device) -> Result<Model, SttError> {
        let config = &whisper.config;
        if config.num_mel_bins != MEL_BINS || vocab.timestamp_begin() as usize + 1501 > config.vocab_size {
            return Err(SttError::Model("the model and its tokenizer do not match".into()));
        }
        let filters = MEL_FILTERS.as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect();
        Ok(Model { whisper, vocab, filters, device })
    }

    /// The encoder's output for one window of samples (at most 30 s).
    fn encode(&mut self, samples: &[i16], cancel: &AtomicBool) -> Result<Tensor, SttError> {
        let pcm: Vec<f32> = samples.iter().map(|sample| f32::from(*sample) / 32768.0).collect();
        // Padded by candle to at least one more window, so always 3000 frames after the cut.
        let mel = audio::pcm_to_mel(&self.whisper.config, &pcm, &self.filters);
        let frames = mel.len() / MEL_BINS;
        let mel = Tensor::from_vec(mel, (1, MEL_BINS, frames), &self.device)?.narrow(2, 0, frames.min(m::N_FRAMES))?;
        self.whisper.encoder.forward(&mel, cancel)
    }

    /// The decoder's output for `tokens`, the ones not given since the last `first`, one row each.
    fn forward(&mut self, tokens: &[u32], features: &Tensor, first: bool) -> Result<Tensor, SttError> {
        let tokens = Tensor::new(tokens, &self.device)?.unsqueeze(0)?;
        self.whisper.decoder.forward(&tokens, features, first)
    }

    fn logits_at(&self, hidden: &Tensor, position: usize) -> Result<Vec<f32>, SttError> {
        Ok(self.whisper.decoder.final_linear(&hidden.i((..1, position..position + 1))?)?.i(0)?.i(0)?.to_vec1::<f32>()?)
    }

    fn detect_language(&mut self, features: &Tensor) -> Result<u32, SttError> {
        let hidden = self.forward(&[self.vocab.sot], features, true)?;
        let logits = self.logits_at(&hidden, 0)?;
        let logit = |id: u32| logits.get(id as usize).copied().unwrap_or(f32::NEG_INFINITY);
        let mut best = self.vocab.languages.first().map(|(_, id)| *id).ok_or_else(|| SttError::Model("no languages".into()))?;
        for (_, id) in &self.vocab.languages {
            if logit(*id) > logit(best) {
                best = *id;
            }
        }
        Ok(best)
    }

    fn decode(&mut self, features: &Tensor, language: u32, temperature: f64, cancel: &AtomicBool) -> Result<Decoding, SttError> {
        let prompt = [self.vocab.sot, language, self.vocab.transcribe];
        let mut tokens = prompt.to_vec();
        let max = self.whisper.config.max_target_positions;
        let suppress = self.whisper.config.suppress_tokens.clone();
        let (mut sum_logprob, mut no_speech_prob) = (0.0, 0.0);
        // Up to Whisper's 448 positions of the decoder, prompt included.
        // The decoder keeps what it has seen: the prompt goes in first, then one token a step.
        let (mut step, mut given) = (0, 0);
        while tokens.len() < max {
            check(cancel)?;
            let hidden = self.forward(tokens.get(given..).unwrap_or_default(), features, step == 0)?;
            let rows = tokens.len() - given;
            given = tokens.len();
            if step == 0 {
                let first = log_softmax(&self.logits_at(&hidden, 0)?);
                no_speech_prob = f64::from(first.get(self.vocab.no_speech as usize).copied().unwrap_or(f32::NEG_INFINITY).exp());
            }
            let mut logits = self.logits_at(&hidden, rows - 1)?;
            timestamp_rules(&self.vocab, &suppress, &mut logits, tokens.get(prompt.len()..).unwrap_or_default());
            let next = if temperature > 0.0 { sample(&logits, temperature)? } else { argmax(&logits) };
            tokens.push(next);
            step += 1;
            if next == self.vocab.eot {
                break;
            }
            sum_logprob += f64::from(log_softmax(&logits).get(next as usize).copied().unwrap_or(f32::NEG_INFINITY));
        }
        let sampled = tokens.split_off(prompt.len());
        let cut = sampled.last() != Some(&self.vocab.eot);
        let compression_ratio = compression_ratio(&self.vocab.decode(&sampled));
        Ok(Decoding { avg_logprob: sum_logprob / sampled.len().max(1) as f64, tokens: sampled, no_speech_prob, compression_ratio, cut })
    }

    /// One window's pieces of speech: start and end in milliseconds from the window's start, and
    /// text. Empty for a window the model takes for silence.
    fn transcribe_window(&mut self, features: &Tensor, language: u32, cancel: &AtomicBool) -> Result<Vec<(u64, u64, String)>, SttError> {
        let mut decoding = None;
        for temperature in m::TEMPERATURES {
            let found = self.decode(features, language, temperature, cancel)?;
            let retry = found.compression_ratio > m::COMPRESSION_RATIO_THRESHOLD || found.avg_logprob < m::LOGPROB_THRESHOLD;
            let done = !retry || found.no_speech_prob > m::NO_SPEECH_THRESHOLD;
            decoding = Some(found);
            if done {
                break;
            }
        }
        let Some(decoding) = decoding.filter(|found| !(found.no_speech_prob > m::NO_SPEECH_THRESHOLD && found.avg_logprob < m::LOGPROB_THRESHOLD))
        else {
            return Ok(Vec::new());
        };
        Ok(pieces(&self.vocab, &decoding.tokens, decoding.cut))
    }

    /// Transcribes a recording's tracks (`others.wav` and `me.wav` in `folder`), with `language`
    /// (a Whisper code) or the language found in each track's first window with sound. The
    /// transcript's language is the one of the first track with speech.
    pub fn transcribe(&mut self, folder: &Path, language: Option<&str>, cancel: &AtomicBool) -> Result<Transcript, SttError> {
        let fixed = match language {
            Some(code) => Some(self.vocab.language(code).ok_or_else(|| SttError::Model(format!("no language {code}")))?),
            None => None,
        };
        let mut cues = Vec::new();
        let mut first_language = None;
        for (track, speaker) in TRACKS.iter().zip([Speaker::Others, Speaker::Me]) {
            let path = folder.join(track);
            if !path.exists() {
                continue;
            }
            let mut reader = wav::Reader::open(&path)?;
            let mut language = fixed;
            let (mut offset, mut samples) = (0u64, Vec::with_capacity(m::N_SAMPLES));
            loop {
                samples.clear();
                if reader.read(&mut samples, m::N_SAMPLES)? == 0 {
                    break;
                }
                let length = samples.len() as u64 * 1000 / u64::from(wav::SAMPLE_RATE);
                if !silent(&samples) {
                    check(cancel)?;
                    let features = self.encode(&samples, cancel)?;
                    let found = match language {
                        Some(found) => found,
                        None => self.detect_language(&features)?,
                    };
                    language = Some(found);
                    first_language = first_language.or(Some(found));
                    for (start, end, text) in self.transcribe_window(&features, found, cancel)? {
                        cues.push(Cue { start_ms: offset + start.min(length), end_ms: offset + end.min(length), speaker, text });
                    }
                }
                offset += length;
            }
        }
        // Stable: at the same time the other side comes first, as the tracks were read.
        cues.sort_by_key(|cue| cue.start_ms);
        Ok(Transcript { language: first_language.and_then(|id| self.vocab.code(id)), cues: cap(cues) })
    }
}

/// Keeps a transcript within its limits (4.3): long cues are cut, and cues past the size of a
/// transcript are left out, the last kept one marked.
fn cap(cues: Vec<Cue>) -> Vec<Cue> {
    // JSON writes a byte as at most 6 (`\u00XX`); 96 more per cue for the times and keys.
    let mut budget = MAX_TRANSCRIPT as usize - 64 * 1024;
    let mut kept: Vec<Cue> = Vec::new();
    for mut cue in cues {
        if cue.text.chars().count() > MAX_CUE_TEXT {
            cue.text = cue.text.chars().take(MAX_CUE_TEXT).collect::<String>() + CUT;
        }
        let cost = cue.text.len() * 6 + 96;
        if kept.len() == MAX_CUES || cost > budget {
            if let Some(last) = kept.last_mut() {
                last.text.push_str(CUT);
            }
            break;
        }
        budget -= cost;
        kept.push(cue);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::DType;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    /// Tokens of the real `tokenizer.json` of whisper-large-v3-turbo (MIT), with their ids.
    const EXCERPT: &str = r#"{
        "model": {"vocab": {",": 11, ".": 13, "Ä": 128, "Ġ": 220, "ĩ": 229, "my": 2226, "ÄĻ": 1274,
            "ĠsiÄĻ": 3244, "ĠÅ¼e": 3561, "ĠktÃ³re": 8864, "ĠteÅ¼": 9516, "Ġdobry": 35884}},
        "added_tokens": [
            {"id": 50257, "content": "<|endoftext|>"}, {"id": 50258, "content": "<|startoftranscript|>"},
            {"id": 50259, "content": "<|en|>"}, {"id": 50269, "content": "<|pl|>"},
            {"id": 50359, "content": "<|translate|>"}, {"id": 50360, "content": "<|transcribe|>"},
            {"id": 50363, "content": "<|nospeech|>"}, {"id": 50364, "content": "<|notimestamps|>"}
        ]}"#;

    #[test]
    fn real_tokens_decode_to_polish_text() {
        let vocab = Vocab::parse(EXCERPT.as_bytes()).unwrap();
        assert_eq!(vocab.decode(&[35884, 11, 3561, 3244, 9516, 13]), " dobry, że się też.");
        // "ć" split over two single-byte tokens, and special tokens left out.
        assert_eq!(vocab.decode(&[50258, 128, 229, 50364, 8864, 220]), "ć które ");
        assert_eq!((vocab.language("pl"), vocab.code(50259).as_deref()), (Some(50269), Some("en")));
        assert_eq!(vocab.timestamp_begin(), 50365);
        // Half a character is replaced, never a panic.
        assert_eq!(vocab.decode(&[128]), "\u{FFFD}");
    }

    #[test]
    fn a_broken_tokenizer_is_refused() {
        for bad in ["", "{}", r#"{"model": {"vocab": {}}, "added_tokens": []}"#] {
            assert!(Vocab::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_mel_filters_are_candles() {
        assert_eq!(MEL_FILTERS.len(), MEL_BINS * 201 * 4);
        let sum: String = Sha256::digest(MEL_FILTERS).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sum, "2a5f9822897750e047c85dea37cc268d3be0ecfd23c28a5f10da129d99d05afe");
    }

    /// A vocabulary of 10 text tokens, then the specials, then 1501 timestamps.
    fn small_vocab() -> Vocab {
        let added: Vec<_> = [
            "<|endoftext|>",
            "<|startoftranscript|>",
            "<|en|>",
            "<|pl|>",
            "<|translate|>",
            "<|transcribe|>",
            "<|startoflm|>",
            "<|startofprev|>",
            "<|nospeech|>",
            "<|notimestamps|>",
        ]
        .iter()
        .enumerate()
        .map(|(i, content)| json!({"id": 10 + i, "content": content}))
        .collect();
        let vocab: serde_json::Map<_, _> = "abcdefghij".chars().enumerate().map(|(i, c)| (c.to_string(), json!(i))).collect();
        Vocab::parse(json!({"model": {"vocab": vocab}, "added_tokens": added}).to_string().as_bytes()).unwrap()
    }

    const SMALL: u32 = 20 + 1501;

    #[test]
    fn timestamps_open_close_and_never_go_back() {
        let vocab = small_vocab();
        let begin = vocab.timestamp_begin();
        let allowed = |sampled: &[u32]| {
            let mut logits = vec![0.0; SMALL as usize];
            timestamp_rules(&vocab, &[], &mut logits, sampled);
            logits.iter().enumerate().filter(|(_, l)| l.is_finite()).map(|(i, _)| i as u32).collect::<Vec<_>>()
        };
        // First a timestamp within the first second.
        assert_eq!(allowed(&[]), (begin..=begin + 50).collect::<Vec<_>>());
        // After the opening timestamp: text or the end, never a special token.
        let after_open = allowed(&[begin + 5]);
        assert!(after_open.iter().all(|t| *t <= vocab.eot) && after_open.contains(&3));
        // After text: no timestamp before the last one.
        let after_text = allowed(&[begin + 5, 3]);
        assert!(after_text.iter().all(|t| *t <= vocab.eot || *t > begin + 5) && !after_text.is_empty());
        // After a closing timestamp: another (it may repeat) or the end, no text.
        let after_close = allowed(&[begin + 5, 3, begin + 9]);
        assert!(after_close.iter().all(|t| *t == vocab.eot || *t >= begin + 9) && after_close.contains(&(begin + 9)));
        // After a closed pair: text or the end.
        let after_pair = allowed(&[begin + 5, 3, begin + 9, begin + 9]);
        assert!(after_pair.iter().all(|t| *t <= vocab.eot) && after_pair.contains(&3));
        // No special token but the end, ever.
        for sampled in [&[][..], &[begin + 5], &[begin + 5, 3]] {
            assert!(allowed(sampled).iter().all(|t| *t <= vocab.eot || *t >= begin));
        }
    }

    #[test]
    fn pieces_follow_the_timestamps_and_a_cut_is_marked() {
        let vocab = small_vocab();
        let begin = vocab.timestamp_begin();
        let tokens = [begin, 0, 1, begin + 50, begin + 50, 2, vocab.eot];
        assert_eq!(pieces(&vocab, &tokens, false), vec![(0, 1000, "ab".to_string()), (1000, u64::MAX, "c".to_string())]);
        // At the token limit: the last piece says it was cut.
        assert_eq!(pieces(&vocab, &tokens[..6], true).last().unwrap().2, format!("c{CUT}"));
    }

    #[test]
    fn looping_text_compresses_well() {
        assert!(compression_ratio(&"Dziękuję. ".repeat(40)) > m::COMPRESSION_RATIO_THRESHOLD);
        assert!(compression_ratio("Dzień dobry, zaczynamy spotkanie o budżecie na przyszły rok.") < m::COMPRESSION_RATIO_THRESHOLD);
    }

    #[test]
    fn a_transcript_stays_within_its_limits() {
        let cue = |text: String| Cue { start_ms: 0, end_ms: 1, speaker: Speaker::Me, text };
        let kept = cap(vec![cue("x".repeat(MAX_CUE_TEXT + 10))]);
        assert_eq!(kept.first().unwrap().text.chars().count(), MAX_CUE_TEXT + CUT.len());
        let kept = cap((0..MAX_CUES + 5).map(|_| cue("a".into())).collect());
        assert_eq!(kept.len(), MAX_CUES);
        assert!(kept.last().unwrap().text.ends_with(CUT));
        let cues = cap((0..5000).map(|_| cue("\u{1}".repeat(MAX_CUE_TEXT))).collect());
        let json = serde_json::to_vec(&Transcript { language: Some("pl".into()), cues }).unwrap();
        assert!(json.len() as u64 <= MAX_TRANSCRIPT);
    }

    #[test]
    fn a_panic_in_a_transcription_is_an_error() {
        assert!(matches!(isolated(|| -> Result<(), SttError> { panic!("boom") }), Err(SttError::Panicked)));
        assert_eq!(isolated(|| Ok(7)).unwrap(), 7);
    }

    /// A model of the real shape but tiny, with zero weights: its output is meaningless, but it
    /// runs every step of a transcription.
    fn zero_model() -> Model {
        let config = Config {
            num_mel_bins: MEL_BINS,
            max_source_positions: 1500,
            d_model: 4,
            encoder_attention_heads: 1,
            encoder_layers: 1,
            vocab_size: SMALL as usize,
            max_target_positions: 8,
            decoder_attention_heads: 1,
            decoder_layers: 1,
            suppress_tokens: Vec::new(),
        };
        let whisper = Whisper::load(&VarBuilder::zeros(DType::F32, &Device::Cpu), config).unwrap();
        Model::new(whisper, small_vocab(), Device::Cpu).unwrap()
    }

    fn track(dir: &Path, name: &str, samples: &[i16]) {
        let mut writer = wav::Writer::create(&dir.join(name)).unwrap();
        writer.write(samples).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn silence_is_skipped_and_sound_goes_through_the_model() {
        let dir = tempfile::tempdir().unwrap();
        // 31 s of silence on the other side, 2 s of noise on the microphone.
        track(dir.path(), "others.wav", &vec![0; 31 * 16_000]);
        let noise: Vec<i16> = (0..32_000).map(|i| if i % 2 == 0 { 3000 } else { -3000 }).collect();
        track(dir.path(), "me.wav", &noise);
        let mut model = zero_model();
        let transcript = model.transcribe(dir.path(), None, &AtomicBool::new(false)).unwrap();
        assert!(transcript.language.is_some());
        for cue in &transcript.cues {
            assert_eq!(cue.speaker, Speaker::Me);
            assert!(cue.start_ms <= cue.end_ms && cue.end_ms <= 2000);
        }
        let silent_only = tempfile::tempdir().unwrap();
        track(silent_only.path(), "others.wav", &vec![0; 16_000]);
        assert!(model.transcribe(silent_only.path(), Some("pl"), &AtomicBool::new(false)).unwrap().cues.is_empty());
        assert!(matches!(model.transcribe(silent_only.path(), Some("xx"), &AtomicBool::new(false)), Err(SttError::Model(_))));
    }

    #[test]
    fn a_cancelled_transcription_stops() {
        let dir = tempfile::tempdir().unwrap();
        track(dir.path(), "me.wav", &(0..16_000).map(|i| if i % 2 == 0 { 3000 } else { -3000 }).collect::<Vec<_>>());
        assert!(matches!(zero_model().transcribe(dir.path(), None, &AtomicBool::new(true)), Err(SttError::Cancelled)));
    }
}
