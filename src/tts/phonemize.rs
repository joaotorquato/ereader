//! Texto → fonemas IPA no dialeto que o Kokoro espera.
//!
//! Dois backends, mesma interface:
//!   * `Cli` (default): roda `espeak-ng -q --ipa -v <lang> --stdin` por chunk.
//!     Um processo por chunk (~10 ms), sem bindgen, sem libclang, sem linkar nada.
//!     É o que roda no Pi sem dor.
//!   * `Ffi` (feature `espeak-ffi`): chama `libespeak-ng` direto. Evita o fork, mas
//!     o espeak tem estado global (não é thread-safe) — como só o worker de TTS
//!     fonemiza, isso é ok. As assinaturas abaixo vêm do `speak_lib.h`; NÃO testei
//!     esta feature aqui (sem libespeak no ambiente de build), trate como rascunho.
//!
//! Pontuação: o espeak descarta pontuação na saída de fonemas, mas o Kokoro usa
//! ela para prosódia/pausas. Fazemos o que a lib `phonemizer` (usada pelo Kokoro
//! original) faz com `preserve_punctuation=True`: quebramos o texto em segmentos
//! nas pontuações, fonemizamos cada segmento como uma linha, e reinserimos a
//! pontuação entre eles. Como bônus, os espaços entre palavras sobrevivem, o que
//! `timing.rs` usa para alinhar fonemas ↔ palavras.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    EnUs,
    PtBr,
}

impl Lang {
    /// Nome de voz do espeak-ng.
    pub fn espeak_voice(self) -> &'static str {
        match self {
            Lang::EnUs => "en-us",
            Lang::PtBr => "pt-br",
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Lang::EnUs => "en-us",
            Lang::PtBr => "pt-br",
        }
    }
    /// Do BCP-47 do livro ('pt-BR', 'pt', 'en', '') — só dois idiomas por enquanto.
    pub fn from_book_lang(bcp47: &str) -> Lang {
        if bcp47.to_ascii_lowercase().starts_with("pt") {
            Lang::PtBr
        } else {
            Lang::EnUs
        }
    }
    /// Da letra inicial do nome da voz Kokoro ('a'=en-us, 'p'=pt-br, ...).
    pub fn from_voice(voice: &str) -> Option<Lang> {
        match voice.chars().next()? {
            'a' | 'b' => Some(Lang::EnUs),
            'p' => Some(Lang::PtBr),
            _ => None,
        }
    }
}

/// Resultado por segmento: a pontuação que veio depois, e os fonemas do segmento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phonemized {
    /// String final para tokenizar (fonemas + pontuação + espaços).
    pub phonemes: String,
    /// Fonemas por palavra, na ordem das palavras do texto de entrada, quando
    /// o espeak devolveu o mesmo número de "palavras" que a entrada tinha.
    /// `None` quando não deu para alinhar (timing.rs cai para proporção por chars).
    pub per_word: Option<Vec<String>>,
}

pub trait Phonemizer: Send {
    fn phonemize(&mut self, text: &str, lang: Lang) -> Result<Phonemized>;
}

// -------------------------------------------------------------------- comum

const PUNCT: &[char] = &[
    ';', ':', ',', '.', '!', '?', '¡', '¿', '—', '…', '"', '«', '»', '“', '”',
];

/// Normalizações leves antes do espeak (o Kokoro original faz bem mais, para
/// números/moedas em inglês; o espeak já lê números nos dois idiomas).
pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '‘' | '’' | '`' | '´' => '\'',
            '–' => '-',
            '\u{00a0}' => ' ',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quebra em (segmento_de_texto, pontuação_que_segue). Segmentos vazios são
/// descartados mas a pontuação deles é anexada ao anterior.
fn split_punct(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seg = String::new();
    for c in text.chars() {
        if PUNCT.contains(&c) {
            let s = seg.trim().to_string();
            seg.clear();
            if s.is_empty() {
                if let Some(last) = out.last_mut() {
                    last.1.push(c);
                }
            } else {
                out.push((s, c.to_string()));
            }
        } else {
            seg.push(c);
        }
    }
    let s = seg.trim().to_string();
    if !s.is_empty() {
        out.push((s, String::new()));
    }
    out
}

/// Pós-processamento que o Kokoro aplica em cima da saída do espeak
/// (`kokoro-onnx/tokenizer.py`, `misaki` EspeakFallback).
pub fn kokoro_fixups(ps: &str, lang: Lang) -> String {
    let mut s = ps
        .replace("kəkˈoːɹoʊ", "kˈoʊkəɹoʊ")
        .replace("kəkˈɔːɹəʊ", "kˈəʊkəɹəʊ")
        .replace('ʲ', "j")
        .replace('r', "ɹ")
        .replace('x', "k")
        .replace('ɬ', "l");
    // espeak usa U+0361 (tie bar) em alguns ditongos e "ˑ"; ambos fora do vocab
    // seriam descartados de qualquer jeito — remover aqui deixa o per_word limpo.
    s.retain(|c| c != '\u{0361}' && c != '\u{0329}');
    if lang == Lang::EnUs {
        // "ninety" → espeak dá "nˈaɪnti", Kokoro foi treinado com "nˈaɪndi".
        s = s.replace("nˈaɪnti", "nˈaɪndi");
    }
    // " z" antes de pontuação/fim → "z" (plural colado na palavra anterior).
    let mut fixed = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' && i + 1 < chars.len() && chars[i + 1] == 'z' {
            let after = chars.get(i + 2).copied();
            if after.is_none()
                || after
                    .map(|c| PUNCT.contains(&c) || c == ' ')
                    .unwrap_or(false)
            {
                i += 1; // pula o espaço
                continue;
            }
        }
        fixed.push(chars[i]);
        i += 1;
    }
    fixed
}

/// Junta segmentos fonemizados em uma string final e, se possível, em fonemas por palavra.
fn assemble(
    segments: &[(String, String)],
    phon_lines: &[String],
    lang: Lang,
    input_words: usize,
) -> Phonemized {
    let mut phonemes = String::new();
    let mut per_word: Vec<String> = Vec::new();
    for ((_, punct), line) in segments.iter().zip(phon_lines) {
        let line = kokoro_fixups(line.trim(), lang);
        if !phonemes.is_empty() && !phonemes.ends_with(' ') {
            phonemes.push(' ');
        }
        phonemes.push_str(&line);
        phonemes.push_str(punct);
        per_word.extend(line.split_whitespace().map(|w| w.to_string()));
    }
    let per_word = if per_word.len() == input_words {
        Some(per_word)
    } else {
        None
    };
    Phonemized {
        phonemes: phonemes.trim().to_string(),
        per_word,
    }
}

fn count_words(segments: &[(String, String)]) -> usize {
    segments
        .iter()
        .map(|(s, _)| s.split_whitespace().count())
        .sum()
}

// --------------------------------------------------------------------- CLI

pub struct Cli {
    bin: String,
}

impl Cli {
    pub fn new(bin: impl Into<String>) -> Self {
        Cli { bin: bin.into() }
    }

    /// Checa que o binário existe e responde; chamado uma vez no boot.
    pub fn check(&self) -> Result<()> {
        let out = Command::new(&self.bin)
            .arg("--version")
            .output()
            .with_context(|| {
                format!(
                    "não consegui executar `{}`; instale espeak-ng ou use --espeak-bin",
                    self.bin
                )
            })?;
        if !out.status.success() {
            bail!("`{} --version` falhou", self.bin);
        }
        Ok(())
    }

    fn run(&self, lines: &[String], lang: Lang) -> Result<Vec<String>> {
        let mut child = Command::new(&self.bin)
            .args(["-q", "--ipa", "-v", lang.espeak_voice(), "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("executando {}", self.bin))?;
        {
            let mut stdin = child.stdin.take().context("stdin do espeak")?;
            for l in lines {
                stdin.write_all(l.as_bytes())?;
                stdin.write_all(b"\n")?;
            }
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("espeak-ng saiu com {}", out.status);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

impl Phonemizer for Cli {
    fn phonemize(&mut self, text: &str, lang: Lang) -> Result<Phonemized> {
        let text = normalize(text);
        let segments = split_punct(&text);
        if segments.is_empty() {
            return Ok(Phonemized {
                phonemes: String::new(),
                per_word: Some(vec![]),
            });
        }
        let lines: Vec<String> = segments.iter().map(|(s, _)| s.clone()).collect();
        let phon = self.run(&lines, lang)?;
        if phon.len() == segments.len() {
            return Ok(assemble(&segments, &phon, lang, count_words(&segments)));
        }
        // O espeak quebrou uma linha em duas (acontece com abreviações tipo "Dr").
        // Fonemiza tudo de uma vez e desiste do alinhamento por palavra.
        tracing::debug!(
            expected = segments.len(),
            got = phon.len(),
            "espeak: linhas divergentes"
        );
        let joined = self.run(&[text.clone()], lang)?.join(" ");
        Ok(Phonemized {
            phonemes: kokoro_fixups(&joined, lang),
            per_word: None,
        })
    }
}

// --------------------------------------------------------------------- FFI

#[cfg(feature = "espeak-ffi")]
pub mod ffi {
    use super::*;
    use std::ffi::{c_char, c_int, c_void, CStr, CString};

    // speak_lib.h — RASCUNHO NÃO COMPILADO/TESTADO. Confira contra o header instalado.
    const AUDIO_OUTPUT_RETRIEVAL: c_int = 1; // não abre dispositivo de áudio
    const ESPEAK_CHARS_UTF8: c_int = 1;
    const ESPEAK_PHONEMES_IPA: c_int = 0x02;

    #[link(name = "espeak-ng")]
    extern "C" {
        fn espeak_Initialize(
            output: c_int,
            buflength: c_int,
            path: *const c_char,
            options: c_int,
        ) -> c_int;
        fn espeak_SetVoiceByName(name: *const c_char) -> c_int;
        fn espeak_TextToPhonemes(
            textptr: *mut *const c_void,
            textmode: c_int,
            phonememode: c_int,
        ) -> *const c_char;
    }

    pub struct Ffi {
        current: Option<Lang>,
    }

    impl Ffi {
        pub fn new() -> Result<Self> {
            // SAFETY: chamada única no boot, antes de qualquer outra função do espeak.
            let rate = unsafe { espeak_Initialize(AUDIO_OUTPUT_RETRIEVAL, 0, std::ptr::null(), 0) };
            if rate < 0 {
                bail!("espeak_Initialize falhou ({rate}); espeak-ng-data instalado?");
            }
            Ok(Ffi { current: None })
        }

        fn set_lang(&mut self, lang: Lang) -> Result<()> {
            if self.current == Some(lang) {
                return Ok(());
            }
            let name = CString::new(lang.espeak_voice())?;
            // SAFETY: ponteiro válido pela duração da chamada.
            let rc = unsafe { espeak_SetVoiceByName(name.as_ptr()) };
            if rc != 0 {
                bail!("espeak_SetVoiceByName({}) = {rc}", lang.espeak_voice());
            }
            self.current = Some(lang);
            Ok(())
        }

        fn one_line(&mut self, text: &str) -> Result<String> {
            let c = CString::new(text)?;
            let mut ptr: *const c_void = c.as_ptr() as *const c_void;
            let mut out = String::new();
            // espeak_TextToPhonemes devolve uma cláusula por vez e avança `ptr`;
            // NULL em `ptr` sinaliza fim do texto.
            while !ptr.is_null() {
                // SAFETY: `c` vive até o fim do loop; a lib devolve buffer estático
                // válido até a próxima chamada, e copiamos antes disso.
                let p = unsafe {
                    espeak_TextToPhonemes(&mut ptr, ESPEAK_CHARS_UTF8, ESPEAK_PHONEMES_IPA)
                };
                if p.is_null() {
                    break;
                }
                let s = unsafe { CStr::from_ptr(p) }.to_string_lossy();
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s.trim());
            }
            Ok(out)
        }
    }

    impl Phonemizer for Ffi {
        fn phonemize(&mut self, text: &str, lang: Lang) -> Result<Phonemized> {
            self.set_lang(lang)?;
            let text = normalize(text);
            let segments = split_punct(&text);
            let mut lines = Vec::with_capacity(segments.len());
            for (s, _) in &segments {
                lines.push(self.one_line(s)?);
            }
            Ok(assemble(&segments, &lines, lang, count_words(&segments)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn espeak_available() -> bool {
        Command::new("espeak-ng")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn split_preserva_pontuacao() {
        let s = split_punct("Olá, mundo. Tudo bem?!");
        assert_eq!(
            s,
            vec![
                ("Olá".to_string(), ",".to_string()),
                ("mundo".to_string(), ".".to_string()),
                ("Tudo bem".to_string(), "?!".to_string()),
            ]
        );
    }

    #[test]
    fn fixups_basicos() {
        assert_eq!(kokoro_fixups("ɹˈɛd r", Lang::EnUs), "ɹˈɛd ɹ");
        assert_eq!(kokoro_fixups("dˈɔɡ z.", Lang::EnUs), "dˈɔɡz.");
        assert_eq!(kokoro_fixups("nˈaɪnti", Lang::EnUs), "nˈaɪndi");
        assert_eq!(kokoro_fixups("nˈaɪnti", Lang::PtBr), "nˈaɪnti");
    }

    /// Frases reais nas duas línguas — só roda se o espeak-ng estiver instalado.
    #[test]
    fn fonemiza_en_e_pt() {
        if !espeak_available() {
            eprintln!("espeak-ng ausente; pulando");
            return;
        }
        let mut p = Cli::new("espeak-ng");
        let en = p
            .phonemize("Hello world, this is a test.", Lang::EnUs)
            .unwrap();
        assert!(
            en.phonemes.contains('ˈ'),
            "sem marca de tônica: {}",
            en.phonemes
        );
        assert!(en.phonemes.ends_with('.'));
        assert!(en.phonemes.contains(','));
        assert_eq!(en.per_word.as_ref().map(|w| w.len()), Some(6));
        assert!(
            !en.phonemes.contains('r'),
            "r deve virar ɹ: {}",
            en.phonemes
        );

        let pt = p
            .phonemize("Olá mundo, isto é um teste.", Lang::PtBr)
            .unwrap();
        assert!(pt.phonemes.contains('ˈ'));
        assert_eq!(pt.per_word.as_ref().map(|w| w.len()), Some(6));
        // tudo que sobrou tem que ser tokenizável (vocab descarta o resto)
        let toks = crate::tts::vocab::tokenize(&pt.phonemes);
        assert!(toks.len() >= pt.phonemes.chars().count() / 2);
    }
}
