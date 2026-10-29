//! Speech to text with Whisper, entirely on this machine (spec §7
//! "on-device live captions").
//!
//! Pure Rust end to end -- candle for the network, and the mel filterbank
//! and tokenizer written out here -- so captions build for every platform
//! the rest of the app does, with no C toolchain and no system library.
//! The audio never leaves the process: there is no network code in this
//! file, and the model is read from local files the user chose to fetch.

use std::collections::HashMap;
use std::path::Path;

use candle_core::{DType, Device, IndexOp, Tensor, D};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self as w, audio, model::Whisper};

use crate::pipeline::Transcriber;

/// Longest caption decoded from one clip, in tokens. A clip is at most a
/// few seconds, so anything near this is the model looping.
const MAX_TOKENS: usize = 96;

pub struct WhisperModel {
    model: Whisper,
    vocab: Vocab,
    filters: Vec<f32>,
    device: Device,
    prompt: Vec<u32>,
    eot: u32,
    no_speech: Option<u32>,
    /// Added to the logits every step: -inf on tokens that must never be
    /// produced (timestamps, task markers, the config's suppress list).
    suppress: Tensor,
}

impl WhisperModel {
    /// Loads `config.json`, `tokenizer.json` and `model.safetensors` from
    /// `dir`.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let err = |what: &str, e: &dyn std::fmt::Display| format!("{what}: {e}");
        let config: w::Config = serde_json::from_slice(
            &std::fs::read(dir.join("config.json")).map_err(|e| err("config.json", &e))?,
        )
        .map_err(|e| err("config.json", &e))?;
        let vocab = Vocab::from_json(
            &std::fs::read(dir.join("tokenizer.json")).map_err(|e| err("tokenizer.json", &e))?,
        )?;
        let weights = std::fs::read(dir.join("model.safetensors")).map_err(|e| err("model.safetensors", &e))?;
        let device = Device::Cpu;
        let vb = VarBuilder::from_buffered_safetensors(weights, w::DTYPE, &device)
            .map_err(|e| err("model.safetensors", &e))?;
        let model = Whisper::load(&vb, config.clone()).map_err(|e| err("model", &e))?;

        let token = |s: &str| vocab.id(s).ok_or_else(|| format!("tokenizer has no {s}"));
        let sot = token(w::SOT_TOKEN)?;
        let eot = token(w::EOT_TOKEN)?;
        let no_timestamps = token(w::NO_TIMESTAMPS_TOKEN)?;
        // English-only models take no language or task tokens; the
        // multilingual ones are told English and "transcribe", since
        // translating someone's words is not captioning them.
        let prompt = match (vocab.id("<|en|>"), vocab.id(w::TRANSCRIBE_TOKEN)) {
            (Some(en), Some(transcribe)) if config.vocab_size > 51_864 => {
                vec![sot, en, transcribe, no_timestamps]
            }
            _ => vec![sot, no_timestamps],
        };
        let no_speech = w::NO_SPEECH_TOKENS.iter().find_map(|t| vocab.id(t));

        let mut mask = vec![0f32; config.vocab_size];
        for &id in &config.suppress_tokens {
            if let Some(m) = mask.get_mut(id as usize) {
                *m = f32::NEG_INFINITY;
            }
        }
        // Every special token after end-of-text: none belongs in a caption.
        for (i, m) in mask.iter_mut().enumerate() {
            if i as u32 > eot {
                *m = f32::NEG_INFINITY;
            }
        }
        let suppress = Tensor::new(mask.as_slice(), &device).map_err(|e| err("mask", &e))?;

        Ok(Self {
            filters: mel_filters(config.num_mel_bins, w::N_FFT, w::SAMPLE_RATE as f64),
            model,
            vocab,
            device,
            prompt,
            eot,
            no_speech,
            suppress,
        })
    }

    fn run(&mut self, pcm: &[f32]) -> candle_core::Result<String> {
        // Whisper always looks at a 30 s window; a short clip is padded
        // with silence, which is what it was trained on.
        let mut padded = pcm[..pcm.len().min(w::N_SAMPLES)].to_vec();
        padded.resize(w::N_SAMPLES, 0.0);
        let mel = audio::pcm_to_mel(&self.model.config, &padded, &self.filters);
        let n_mels = self.model.config.num_mel_bins;
        let frames = mel.len() / n_mels;
        let mel = Tensor::from_vec(mel, (1, n_mels, frames), &self.device)?.narrow(2, 0, w::N_FRAMES)?;

        let features = self.model.encoder.forward(&mel, true)?;
        let mut tokens = self.prompt.clone();
        let mut sum_logprob = 0.0f64;
        let mut no_speech_prob = 0.0f64;
        let mut produced = 0usize;
        for i in 0..MAX_TOKENS {
            let input = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
            let ys = self.model.decoder.forward(&input, &features, i == 0)?;
            let (_, seq_len, _) = ys.dims3()?;
            let logits = self.model.decoder.final_linear(&ys.i((..1, seq_len - 1..))?)?.i(0)?.i(0)?;

            if i == 0 {
                if let Some(ns) = self.no_speech {
                    let probs = candle_nn::ops::softmax(&logits, D::Minus1)?;
                    no_speech_prob = probs.i(ns as usize)?.to_scalar::<f32>()? as f64;
                }
            }
            let logits = logits.broadcast_add(&self.suppress)?;
            let next = logits.argmax(D::Minus1)?.to_scalar::<u32>()?;
            let logprobs = candle_nn::ops::log_softmax(&logits, D::Minus1)?;
            sum_logprob += logprobs.i(next as usize)?.to_dtype(DType::F64)?.to_scalar::<f64>()?;
            if next == self.eot {
                break;
            }
            tokens.push(next);
            produced += 1;
        }

        // Whisper's own rule for "this was not speech": the model thinks
        // so *and* is unsure of the words it came up with. Without it,
        // a cough becomes "Thank you." on screen.
        let avg_logprob = sum_logprob / (produced.max(1) as f64);
        if no_speech_prob > w::NO_SPEECH_THRESHOLD && avg_logprob < w::LOGPROB_THRESHOLD {
            return Ok(String::new());
        }
        Ok(self.vocab.decode(&tokens[self.prompt.len()..]))
    }
}

impl Transcriber for WhisperModel {
    fn transcribe(&mut self, pcm16k: &[f32]) -> Result<String, String> {
        self.run(pcm16k).map_err(|e| e.to_string())
    }
}

/// The decoding half of Whisper's byte-level BPE tokenizer.
///
/// Captions only ever turn ids into text, never text into ids, so the
/// merge rules are not needed: each id maps to a string of stand-in
/// characters, one per byte, and GPT-2's byte table maps them back. That
/// is all the `tokenizers` crate would do here, without its C regex
/// dependency.
pub struct Vocab {
    by_id: HashMap<u32, String>,
    by_text: HashMap<String, u32>,
    special: std::collections::HashSet<u32>,
}

impl Vocab {
    pub fn from_json(json: &[u8]) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| format!("tokenizer.json: {e}"))?;
        let mut by_id = HashMap::new();
        let mut by_text = HashMap::new();
        let mut special = std::collections::HashSet::new();
        let vocab = v["model"]["vocab"].as_object().ok_or("tokenizer.json: no model.vocab")?;
        for (text, id) in vocab {
            let id = id.as_u64().ok_or("tokenizer.json: bad id")? as u32;
            by_id.insert(id, text.clone());
            by_text.insert(text.clone(), id);
        }
        if let Some(added) = v["added_tokens"].as_array() {
            for t in added {
                let (Some(id), Some(text)) = (t["id"].as_u64(), t["content"].as_str()) else { continue };
                by_id.insert(id as u32, text.to_string());
                by_text.insert(text.to_string(), id as u32);
                if t["special"].as_bool().unwrap_or(true) {
                    special.insert(id as u32);
                }
            }
        }
        Ok(Self { by_id, by_text, special })
    }

    pub fn id(&self, text: &str) -> Option<u32> {
        self.by_text.get(text).copied()
    }

    pub fn decode(&self, ids: &[u32]) -> String {
        let table = byte_decoder();
        let mut bytes = Vec::new();
        for id in ids {
            if self.special.contains(id) {
                continue;
            }
            let Some(text) = self.by_id.get(id) else { continue };
            for c in text.chars() {
                match table.get(&c) {
                    Some(b) => bytes.push(*b),
                    None => bytes.extend_from_slice(c.to_string().as_bytes()),
                }
            }
        }
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

/// GPT-2's reversible byte <-> printable character table, inverted.
fn byte_decoder() -> HashMap<char, u8> {
    let mut printable: Vec<u32> = (b'!' as u32..=b'~' as u32)
        .chain(0xA1..=0xAC)
        .chain(0xAE..=0xFF)
        .collect();
    let mut chars = printable.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !printable.contains(&b) {
            printable.push(b);
            chars.push(256 + n);
            n += 1;
        }
    }
    printable
        .into_iter()
        .zip(chars)
        .map(|(b, c)| (char::from_u32(c).unwrap(), b as u8))
        .collect()
}

/// The mel filterbank Whisper was trained with: librosa's
/// `filters.mel(sr, n_fft, n_mels)` with its defaults (Slaney mel scale,
/// Slaney area normalisation), laid out `n_mels` rows of `n_fft/2 + 1`.
/// Computed rather than shipped as a binary blob, so there is nothing
/// opaque in the source tree.
pub fn mel_filters(n_mels: usize, n_fft: usize, sample_rate: f64) -> Vec<f32> {
    fn hz_to_mel(f: f64) -> f64 {
        let f_sp = 200.0 / 3.0;
        let min_log_hz = 1000.0;
        let logstep = (6.4f64).ln() / 27.0;
        if f >= min_log_hz {
            min_log_hz / f_sp + (f / min_log_hz).ln() / logstep
        } else {
            f / f_sp
        }
    }
    fn mel_to_hz(m: f64) -> f64 {
        let f_sp = 200.0 / 3.0;
        let min_log_hz = 1000.0;
        let min_log_mel = min_log_hz / f_sp;
        let logstep = (6.4f64).ln() / 27.0;
        if m >= min_log_mel {
            min_log_hz * (logstep * (m - min_log_mel)).exp()
        } else {
            f_sp * m
        }
    }
    let bins = n_fft / 2 + 1;
    let fft_freqs: Vec<f64> = (0..bins).map(|k| k as f64 * sample_rate / n_fft as f64).collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(sample_rate / 2.0));
    let mel_f: Vec<f64> = (0..n_mels + 2)
        .map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (n_mels + 1) as f64))
        .collect();
    let mut out = vec![0f32; n_mels * bins];
    for i in 0..n_mels {
        let (left, centre, right) = (mel_f[i], mel_f[i + 1], mel_f[i + 2]);
        let norm = 2.0 / (right - left);
        for (k, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - left) / (centre - left);
            let upper = (right - f) / (right - centre);
            out[i * bins + k] = (lower.min(upper).max(0.0) * norm) as f32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filterbank_has_whisper_shape_and_rises_in_pitch() {
        let f = mel_filters(80, 400, 16_000.0);
        assert_eq!(f.len(), 80 * 201);
        let mut last_peak = 0;
        for row in f.chunks(201) {
            assert!(row.iter().all(|v| *v >= 0.0));
            let peak = row
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            assert!(peak >= last_peak);
            last_peak = peak;
        }
        // Whisper's shipped mel_filters.npz starts with 0.02486259 at
        // [0][1]: the first filter peaks on the first non-DC bin.
        assert!((f[1] - 0.024_862_6).abs() < 1e-5, "{}", f[1]);
    }

    #[test]
    fn vocab_decodes_byte_level_tokens() {
        let json = r#"{
            "model": {"vocab": {"Hello": 0, "\u0120world": 1, "\u00c3\u00a9": 2}},
            "added_tokens": [{"id": 3, "content": "<|endoftext|>", "special": true}]
        }"#
        .as_bytes();
        let v = Vocab::from_json(json).unwrap();
        assert_eq!(v.decode(&[0, 1, 3]), "Hello world");
        // A two-byte UTF-8 character spelled as two stand-in characters.
        assert_eq!(v.decode(&[2]), "é");
        assert_eq!(v.id("<|endoftext|>"), Some(3));
    }

    /// The whole inference path -- weights file, config, tokenizer, mel,
    /// encoder, greedy decode, detokenise -- on a miniature Whisper with
    /// random weights, written out in the real file formats. It says
    /// nothing about accuracy (that needs the real model, below), but it
    /// does prove the plumbing loads and runs without the 150 MB download.
    #[test]
    fn miniature_model_runs_end_to_end() {
        let dir = std::env::temp_dir().join(format!("antiphon-whisper-mini-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = serde_json::json!({
            "num_mel_bins": 80, "max_source_positions": 1500, "d_model": 8,
            "encoder_attention_heads": 2, "encoder_layers": 1, "vocab_size": 40,
            "max_target_positions": 128, "decoder_attention_heads": 2,
            "decoder_layers": 1, "suppress_tokens": [1]
        });
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        let mut vocab = serde_json::Map::new();
        for (i, c) in ('a'..='z').enumerate() {
            vocab.insert(c.to_string(), (i as u32).into());
        }
        let specials = ["<|endoftext|>", "<|startoftranscript|>", "<|notimestamps|>", "<|nocaptions|>"];
        let added: Vec<_> = specials
            .iter()
            .enumerate()
            .map(|(i, t)| serde_json::json!({"id": 30 + i, "content": t, "special": true}))
            .collect();
        let tok = serde_json::json!({"model": {"vocab": vocab}, "added_tokens": added});
        std::fs::write(dir.join("tokenizer.json"), tok.to_string()).unwrap();

        let cfg: w::Config = serde_json::from_value(config).unwrap();
        let varmap = candle_nn::VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, w::DTYPE, &Device::Cpu);
        Whisper::load(&vb, cfg).unwrap();
        varmap.save(dir.join("model.safetensors")).unwrap();

        let mut m = WhisperModel::load(&dir).unwrap();
        let tone: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.1).sin() * 0.3).collect();
        let text = m.transcribe(&tone).unwrap();
        // Random weights say random things, but only ever ordinary
        // tokens: no specials, nothing suppressed ("b" is id 1).
        assert!(text.chars().all(|c| c.is_ascii_lowercase() && c != 'b'), "{text:?}");
        assert!(text.len() <= MAX_TOKENS);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Needs the model on disk; see the crate docs for how to fetch it.
    ///
    /// ```text
    /// ANTIPHON_WHISPER_DIR=/path/to/whisper-tiny.en \
    ///     cargo test -p antiphon-captions --release -- --ignored real_model
    /// ```
    ///
    /// The directory holds `config.json`, `tokenizer.json` and
    /// `model.safetensors` from openai/whisper-tiny.en. Feeds it a tone
    /// (expects no caption, or a short one) and, if `ANTIPHON_WHISPER_WAV`
    /// names a 16 kHz mono 16-bit WAV of speech, prints its transcript.
    #[test]
    #[ignore]
    fn real_model() {
        let dir = std::env::var("ANTIPHON_WHISPER_DIR").expect("set ANTIPHON_WHISPER_DIR");
        let mut m = WhisperModel::load(Path::new(&dir)).unwrap();
        let silence = vec![0f32; 16_000 * 2];
        let t = std::time::Instant::now();
        let text = m.transcribe(&silence).unwrap();
        println!("silence -> {text:?} in {:?}", t.elapsed());
        assert!(text.len() < 40, "hallucinated {text:?}");
        if let Ok(wav) = std::env::var("ANTIPHON_WHISPER_WAV") {
            let bytes = std::fs::read(wav).unwrap();
            let pcm: Vec<f32> = bytes[44..]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .collect();
            let t = std::time::Instant::now();
            let text = m.transcribe(&pcm).unwrap();
            println!("speech -> {text:?} in {:?}", t.elapsed());
            assert!(!text.is_empty());
        }
    }
}
