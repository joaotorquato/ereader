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

/// O `speed` do Kokoro é um único escalar que o grafo ONNX aplica a TODAS as
/// durations previstas (fonemas e pontuação/espaço juntos) — não dá pra pedir
/// ao modelo pra acelerar só a fala. Aqui devolvemos as pausas de pontuação
/// e espaço pra duração que teriam em speed=1, mexendo só no áudio já gerado
/// (sem re-inferir): repete os samples que a própria pausa já tem até completar
/// a duração natural (ou corta, se speed<1 tiver alongado). A fala em si fica
/// intocada, na velocidade pedida. Exige o modelo "timestamped" (`durations`).
pub fn restore_pause_speed(out: &Synth, speed: f32) -> (Vec<f32>, Option<Vec<f32>>) {
    let Some(durations) = out.durations.as_deref() else {
        return (out.samples.clone(), None);
    };
    if (speed - 1.0).abs() < 1e-3 || durations.len() != out.token_ids.len() {
        return (out.samples.clone(), Some(durations.to_vec()));
    }
    let total_frames: f32 = durations.iter().sum();
    if total_frames <= 0.0 {
        return (out.samples.clone(), Some(durations.to_vec()));
    }
    // samples/frame calculado do próprio áudio (não chumbado) — robusto a
    // mudanças de hop length entre exports do modelo.
    let samples_per_frame = (out.samples.len() as f64 / total_frames as f64)
        .round()
        .max(1.0) as usize;
    let space_id = vocab::vocab()[&' '];
    let punct_ids = vocab::punct_ids();
    let n = out.token_ids.len();

    let mut samples = Vec::with_capacity(out.samples.len());
    let mut out_durations = Vec::with_capacity(durations.len());
    let mut cursor = 0usize;
    for (i, &fr) in durations.iter().enumerate() {
        let observed = (fr.round().max(0.0) as usize).saturating_mul(samples_per_frame);
        let start = cursor.min(out.samples.len());
        let end = (start + observed).min(out.samples.len());
        cursor = end;
        let seg = &out.samples[start..end];

        let is_pause =
            i == 0 || i == n - 1 || out.token_ids[i] == space_id || punct_ids.contains(&out.token_ids[i]);
        if !is_pause || seg.is_empty() {
            samples.extend_from_slice(seg);
            out_durations.push(fr);
            continue;
        }

        let natural_len =
            ((fr * speed).round().max(0.0) as usize).saturating_mul(samples_per_frame);
        if natural_len >= seg.len() {
            samples.extend_from_slice(seg);
            let extra = natural_len - seg.len();
            samples.extend((0..extra).map(|k| seg[k % seg.len()]));
        } else {
            samples.extend_from_slice(&seg[..natural_len]);
        }
        // grava o MESMO valor arredondado usado no áudio, não `fr * speed` cru —
        // senão o erro de arredondamento (até meio frame) se acumula a cada
        // pausa/espaço do parágrafo inteiro e o highlight dessincroniza do áudio.
        out_durations.push(natural_len as f32 / samples_per_frame as f32);
    }
    if cursor < out.samples.len() {
        samples.extend_from_slice(&out.samples[cursor..]);
    }
    (samples, Some(out_durations))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// "aa ." tokenizado: [pad, a, a, espaço, ponto, pad], 2 samples/frame.
    /// samples[i] = i, pra rastrear exatamente o que foi preservado vs. repetido.
    fn synth_fixture() -> Synth {
        let v = vocab::vocab();
        let token_ids = vec![0, v[&'a'], v[&'a'], v[&' '], v[&'.'], 0];
        let durations = vec![2.0, 10.0, 10.0, 4.0, 6.0, 2.0]; // soma 34 * 2 samples/frame = 68
        let samples = (0..68).map(|i| i as f32).collect();
        Synth {
            samples,
            token_ids,
            durations: Some(durations),
        }
    }

    #[test]
    fn restaura_pausas_e_preserva_fala_ao_dobrar_a_velocidade() {
        let s = synth_fixture();
        let (samples, durations) = restore_pause_speed(&s, 2.0);
        let durations = durations.unwrap();

        // pausas (pads, espaço, ponto) voltam à duração natural (fr * speed)
        assert_eq!(durations, vec![4.0, 10.0, 10.0, 8.0, 12.0, 4.0]);
        // fala (as duas ocorrências de "a") fica intocada, byte a byte
        assert_eq!(&samples[8..28], &s.samples[4..24]);
        assert_eq!(&samples[28..48], &s.samples[24..44]);
        // pausa do espaço: 8 samples originais (44..52) + os mesmos 8 repetidos
        assert_eq!(&samples[48..56], &s.samples[44..52]);
        assert_eq!(&samples[56..64], &s.samples[44..52]);
        assert_eq!(samples.len(), 96);
    }

    /// Regressão: durations fracionárias (o caso real — o modelo não prevê só
    /// inteiros) não podem gravar o valor cru `fr*speed` no timing, senão o
    /// arredondamento do áudio (que É inteiro, em samples) diverge da métrica
    /// usada pro highlight — e diverge de novo a cada pausa do parágrafo,
    /// acumulando até o highlight terminar bem antes do áudio.
    #[test]
    fn duration_da_pausa_bate_com_o_arredondamento_usado_no_audio() {
        let v = vocab::vocab();
        let token_ids = vec![0, v[&'a'], v[&' '], v[&'a'], 0];
        // pausa central com 3.0 frames; a 1.3x soaria 3.9 frames — sem o fix,
        // isso é o que ia pro JSON de timing, mas o áudio só tem os 4 frames
        // (2 samples/frame) redondos que de fato foram inseridos.
        let durations = vec![2.0, 5.0, 3.0, 5.0, 2.0];
        let samples = (0..34).map(|i| i as f32).collect();
        let s = Synth { samples, token_ids, durations: Some(durations) };

        let (samples, durations) = restore_pause_speed(&s, 1.3);
        let durations = durations.unwrap();

        assert_eq!(durations[2], 4.0, "3.0 * 1.3 = 3.9 → arredonda pra 4 frames, não fica cru em 3.9");
        // a duration gravada tem que corresponder exatamente aos samples entregues
        let samples_per_frame = 2;
        let total: f32 = durations.iter().sum();
        assert_eq!((total * samples_per_frame as f32).round() as usize, samples.len());
    }

    #[test]
    fn nao_mexe_em_nada_quando_speed_e_1() {
        let s = synth_fixture();
        let (samples, durations) = restore_pause_speed(&s, 1.0);
        assert_eq!(samples, s.samples);
        assert_eq!(durations.unwrap(), *s.durations.as_ref().unwrap());
    }

    #[test]
    fn sem_durations_do_modelo_devolve_amostras_como_vieram() {
        let mut s = synth_fixture();
        s.durations = None;
        let (samples, durations) = restore_pause_speed(&s, 1.5);
        assert_eq!(samples, s.samples);
        assert!(durations.is_none());
    }
}
