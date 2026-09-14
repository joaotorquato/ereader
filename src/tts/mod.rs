//! TTS: texto → fonemas (espeak-ng) → tokens → Kokoro (ort) → WAV + timings.
//!
//! `chunk`     divide parágrafos em pedaços ≤ 220 chars (com offsets UTF-16)
//! `phonemize` espeak-ng (CLI ou FFI) + ajustes que o Kokoro espera
//! `vocab`     mapa fonema → id
//! `voices`    style vectors (raw .bin por voz ou NPZ)
//! `kokoro`    sessão ONNX e síntese de um chunk
//! `timing`    distribuição de tempo por palavra (real ou proporcional)
//! `queue`     worker único + cache + dedup + prefetch

pub mod chunk;
pub mod kokoro;
pub mod phonemize;
pub mod queue;
pub mod timing;
pub mod vocab;
pub mod voices;

use phonemize::Lang;

/// Voz padrão por idioma; vem de ENV/flags (config.rs).
#[derive(Debug, Clone)]
pub struct VoiceDefaults {
    pub en: String,
    pub pt: String,
}

impl VoiceDefaults {
    pub fn for_lang(&self, lang: Lang) -> &str {
        match lang {
            Lang::EnUs => &self.en,
            Lang::PtBr => &self.pt,
        }
    }
}

/// Resolve (voz, idioma) para um parágrafo: voz explícita do cliente ganha; senão
/// a padrão do idioma do livro. O idioma do fonemizador segue a VOZ (uma voz
/// `pf_*` lendo um livro em inglês fonemiza como pt-br — é o que o Kokoro faz).
pub fn resolve_voice(
    defaults: &VoiceDefaults,
    book_lang: &str,
    requested: Option<&str>,
) -> (String, Lang) {
    let book = Lang::from_book_lang(book_lang);
    let voice = requested
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| defaults.for_lang(book).to_string());
    let lang = Lang::from_voice(&voice).unwrap_or(book);
    (voice, lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_voz() {
        let d = VoiceDefaults {
            en: "af_heart".into(),
            pt: "pf_dora".into(),
        };
        assert_eq!(
            resolve_voice(&d, "pt-BR", None),
            ("pf_dora".into(), Lang::PtBr)
        );
        assert_eq!(
            resolve_voice(&d, "en", None),
            ("af_heart".into(), Lang::EnUs)
        );
        assert_eq!(
            resolve_voice(&d, "", Some("")),
            ("af_heart".into(), Lang::EnUs)
        );
        assert_eq!(
            resolve_voice(&d, "en", Some("pm_alex")),
            ("pm_alex".into(), Lang::PtBr)
        );
        // voz de idioma que não fonemizamos (japonês) cai no idioma do livro
        assert_eq!(
            resolve_voice(&d, "pt", Some("jf_alpha")),
            ("jf_alpha".into(), Lang::PtBr)
        );
    }
}
