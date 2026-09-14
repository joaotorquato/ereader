//! Inferência do Kokoro-82M via `ort` (ONNX Runtime).
//!
//! Contrato do modelo (v1.0):
//!   entradas: `tokens` ou `input_ids` (int64 [1, N], com 0 nas duas pontas),
//!             `style` (f32 [1, 256]), `speed` (f32 [1]);
//!   saídas:   `audio` ou `waveform` (f32, 24 kHz mono) e, no modelo
//!             "timestamped", `durations` (f32, um por token — frames de 600 samples).
//! Descobrimos os nomes olhando `session.inputs()/outputs()` no boot em vez de
//! chumbar, para o mesmo binário aceitar as duas exportações.
//!
//! Ownership: `Kokoro` NÃO é `Sync` nem precisa ser — `Session::run` exige `&mut`,
//! e só existe um worker de inferência (queue.rs), que é dono da instância. Zero
//! Arc<Mutex<Session>>: quem quer áudio manda mensagem para o worker.
//!
//! APIs do ort abaixo seguem o que o Kokoros compila contra `2.0.0-rc.11`. Se sua
//! versão do ort divergir (a série rc muda assinatura entre rcs), os pontos
//! sensíveis são `Tensor::from_array`, `SessionInputs::from` e `try_extract_tensor`.

use super::vocab;
use anyhow::{anyhow, bail, Context, Result};
use ort::session::builder::SessionBuilder;
use ort::session::{Session, SessionInputValue, SessionInputs};
use ort::value::{Tensor, Value};
use std::borrow::Cow;
use std::path::Path;

pub const SAMPLE_RATE: u32 = 24_000;

pub struct Kokoro {
    session: Session,
    tokens_name: &'static str,
    audio_name: String,
    durations_name: Option<String>,
}

pub struct Synth {
    /// PCM f32 em [-1, 1], 24 kHz mono.
    pub samples: Vec<f32>,
    /// ids enviados (com pads) — timing.rs precisa deles junto com `durations`.
    pub token_ids: Vec<i64>,
    /// Frames por token, se o modelo expõe.
    pub durations: Option<Vec<f32>>,
}

impl Kokoro {
    pub fn load(model_path: &Path, intra_threads: usize) -> Result<Self> {
        let session = SessionBuilder::new()
            .map_err(|e| anyhow!("ort SessionBuilder: {e}"))?
            .with_intra_threads(intra_threads.max(1))
            .map_err(|e| anyhow!("ort intra_threads: {e}"))?
            .commit_from_file(model_path)
            .with_context(|| format!("carregando modelo {}", model_path.display()))?;

        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        let output_names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        tracing::info!(?input_names, ?output_names, "modelo carregado");

        let tokens_name = if input_names.iter().any(|n| n == "input_ids") {
            "input_ids"
        } else if input_names.iter().any(|n| n == "tokens") {
            "tokens"
        } else {
            bail!("modelo sem entrada `tokens`/`input_ids`: {input_names:?}");
        };
        let audio_name = ["audio", "waveform", "waveforms"]
            .iter()
            .find(|n| output_names.iter().any(|o| o == *n))
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("modelo sem saída de áudio: {output_names:?}"))?;
        let durations_name = output_names
            .iter()
            .find(|o| o.as_str() == "durations")
            .cloned();

        Ok(Kokoro {
            session,
            tokens_name,
            audio_name,
            durations_name,
        })
    }

    pub fn has_durations(&self) -> bool {
        self.durations_name.is_some()
    }

    /// `phonemes` já pós-processados (phonemize.rs); `style` = voices.style(voice, n_tokens).
    pub fn synth(
        &mut self,
        phonemes: &str,
        style_for: impl FnOnce(usize) -> Result<Vec<f32>>,
        speed: f32,
    ) -> Result<Synth> {
        let tokens = vocab::tokenize(phonemes);
        if tokens.is_empty() {
            bail!("nenhum token para sintetizar");
        }
        let style = style_for(tokens.len())?;
        if style.len() != 256 {
            bail!("style vector com {} floats", style.len());
        }

        let mut ids = Vec::with_capacity(tokens.len() + 2);
        ids.push(0);
        ids.extend_from_slice(&tokens);
        ids.push(0);

        let tokens_tensor = Tensor::from_array(([1usize, ids.len()], ids.clone()))
            .map_err(|e| anyhow!("tensor tokens: {e}"))?;
        let style_tensor = Tensor::from_array(([1usize, 256usize], style))
            .map_err(|e| anyhow!("tensor style: {e}"))?;
        let speed_tensor = Tensor::from_array(([1usize], vec![speed]))
            .map_err(|e| anyhow!("tensor speed: {e}"))?;

        let inputs: Vec<(Cow<'static, str>, SessionInputValue<'static>)> = vec![
            (
                Cow::Borrowed(self.tokens_name),
                SessionInputValue::Owned(Value::from(tokens_tensor)),
            ),
            (
                Cow::Borrowed("style"),
                SessionInputValue::Owned(Value::from(style_tensor)),
            ),
            (
                Cow::Borrowed("speed"),
                SessionInputValue::Owned(Value::from(speed_tensor)),
            ),
        ];

        let outputs = self
            .session
            .run(SessionInputs::from(inputs))
            .map_err(|e| anyhow!("ort run: {e}"))?;

        let samples: Vec<f32> = {
            let (_shape, data) = outputs[self.audio_name.as_str()]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("saída de áudio: {e}"))?;
            data.to_vec()
        };
        let durations = match &self.durations_name {
            Some(name) => {
                let (_shape, data) = outputs[name.as_str()]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| anyhow!("saída durations: {e}"))?;
                Some(data.to_vec())
            }
            None => None,
        };

        Ok(Synth {
            samples,
            token_ids: ids,
            durations,
        })
    }
}

/// f32 → WAV 16-bit PCM mono 24 kHz. 16 bits em vez de f32 porque o Safari toca
/// os dois mas o arquivo fica metade do tamanho; e o Kokoro não tem 24 bits de
/// informação de qualquer jeito.
/// TODO(opus): se o tráfego pelo Tailscale pesar, trocar por Ogg/Opus (crate
/// `opus` + `ogg`); iOS ≥ 17 toca Opus em <audio>. WAV é ~10× maior.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<u32> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        w.write_sample(v)?;
    }
    w.finalize()?;
    Ok(duration_ms(samples.len()))
}

pub fn duration_ms(n_samples: usize) -> u32 {
    ((n_samples as u64) * 1000 / SAMPLE_RATE as u64) as u32
}
