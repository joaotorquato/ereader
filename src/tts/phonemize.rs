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
//! ela para prosódia/pausas. Quebramos cada palavra em pedaços (texto | pontuação),
//! fonemizamos os pedaços de texto e recolocamos a pontuação no lugar.
//!
//! Alinhamento: `per_word` tem exatamente uma entrada por palavra do texto (fonemas
//! + pontuação colada), e `phonemes` é `per_word.join(" ")`. É isso que `timing.rs`
//! usa para saber quais tokens pertencem a qual palavra. Para manter a prosódia,
//! a 1ª passada fonemiza por cláusula (pedaços entre pontuações, com contexto);
//! quando o espeak devolve outro número de palavras que a cláusula tinha (ele
//! funde "there were" → "ðɛɹwˌɜː", lê "2024" em três palavras), só essa cláusula
//! é refeita um pedaço por linha (o espeak não funde palavras entre linhas).

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phonemized {
    /// String final para tokenizar (fonemas + pontuação + espaços).
    pub phonemes: String,
    /// Uma entrada por palavra do texto (fonemas + pontuação colada; pode ter
    /// espaços, ex. "2024" → "tˈuː θˈaʊzənd twˈɛnti fˈɔːɹ"). `None` quando o
    /// espeak devolveu um nº de linhas diferente do pedido (timing.rs cai para
    /// proporção por chars).
    pub per_word: Option<Vec<String>>,
}

pub trait Phonemizer: Send {
    /// Uma saída por entrada, na mesma ordem, sem fundir nem quebrar linhas.
    fn lines(&mut self, inputs: &[String], lang: Lang) -> Result<Vec<String>>;

    fn phonemize(&mut self, text: &str, lang: Lang) -> Result<Phonemized> {
        phonemize_words(self, text, lang)
    }
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

/// Uma palavra → pedaços (texto, é_pontuação), na ordem. "concerns—something"
/// vira [("concerns",f), ("—",t), ("something",f)].
fn word_pieces(word: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for c in word.chars() {
        let p = PUNCT.contains(&c);
        match out.last_mut() {
            Some((s, was)) if *was == p => s.push(c),
            _ => out.push((c.to_string(), p)),
        }
    }
    out
}

fn phonemize_words<P: Phonemizer + ?Sized>(be: &mut P, text: &str, lang: Lang) -> Result<Phonemized> {
    let text = normalize(text);
    let words: Vec<Vec<(String, bool)>> = text.split_whitespace().map(word_pieces).collect();
    // pedaços de texto em ordem, com (palavra, índice do pedaço)
    let mut flat: Vec<(usize, usize)> = Vec::new();
    // cláusulas = runs de pedaços de texto entre pontuações (atravessam palavras)
    let mut clauses: Vec<Vec<usize>> = vec![vec![]];
    for (wi, w) in words.iter().enumerate() {
        for (pi, (_, punct)) in w.iter().enumerate() {
            if *punct {
                if !clauses.last().unwrap().is_empty() {
                    clauses.push(vec![]);
                }
            } else {
                clauses.last_mut().unwrap().push(flat.len());
                flat.push((wi, pi));
            }
        }
    }
    clauses.retain(|c| !c.is_empty());
    let piece_text = |f: usize| words[flat[f].0][flat[f].1].0.as_str();

    let mut ph: Vec<Option<String>> = vec![None; flat.len()];
    if !clauses.is_empty() {
        // 1ª passada: por cláusula, com contexto
        let inputs: Vec<String> = clauses
            .iter()
            .map(|c| c.iter().map(|&f| piece_text(f)).collect::<Vec<_>>().join(" "))
            .collect();
        let out = be.lines(&inputs, lang)?;
        if out.len() != inputs.len() {
            return Ok(unaligned(&out, lang, inputs.len(), out.len()));
        }
        let mut retry: Vec<usize> = Vec::new();
        for (c, line) in clauses.iter().zip(&out) {
            let ws: Vec<&str> = line.split_whitespace().collect();
            if c.len() == 1 {
                ph[c[0]] = Some(line.clone());
            } else if ws.len() == c.len() {
                for (&f, w) in c.iter().zip(ws) {
                    ph[f] = Some(w.to_string());
                }
            } else {
                retry.extend_from_slice(c);
            }
        }
        // 2ª passada: só os pedaços das cláusulas que o espeak fundiu/expandiu
        if !retry.is_empty() {
            let inputs: Vec<String> = retry.iter().map(|&f| piece_text(f).to_string()).collect();
            let out = be.lines(&inputs, lang)?;
            if out.len() != inputs.len() {
                return Ok(unaligned(&out, lang, inputs.len(), out.len()));
            }
            for (&f, line) in retry.iter().zip(out) {
                ph[f] = Some(line);
            }
        }
    }

    let mut per_word: Vec<String> = Vec::with_capacity(words.len());
    let mut f = 0usize;
    for w in &words {
        let mut s = String::new();
        for (t, punct) in w {
            if *punct {
                s.push_str(t);
            } else {
                s.push_str(ph[f].as_deref().unwrap_or(""));
                f += 1;
            }
        }
        per_word.push(kokoro_fixups(s.trim(), lang));
    }
    Ok(Phonemized {
        phonemes: per_word.join(" "),
        per_word: Some(per_word),
    })
}

/// Espeak devolveu outro nº de linhas: fica com os fonemas, sem alinhamento.
fn unaligned(lines: &[String], lang: Lang, expected: usize, got: usize) -> Phonemized {
    tracing::debug!(expected, got, "espeak: linhas divergentes");
    Phonemized {
        phonemes: kokoro_fixups(&lines.join(" "), lang),
        per_word: None,
    }
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
            .args(["-q", "--ipa", "-v", lang.espeak_voice()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("executando {}", self.bin))?;
        {
            let mut stdin = child.stdin.take().context("stdin do espeak")?;
            // Sem `--stdin` o espeak processa linha a linha: uma saída por
            // entrada, sem fundir palavras entre linhas (com `--stdin` ele
            // juntaria tudo numa cláusula e "there were" viraria "ðɛɹwˌɜː").
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
        // Não filtrar linhas vazias: "(" sozinho vira linha vazia e a contagem
        // de saídas tem que bater com a de entradas.
        Ok(text.lines().map(|l| l.trim().to_string()).collect())
    }
}

impl Phonemizer for Cli {
    fn lines(&mut self, inputs: &[String], lang: Lang) -> Result<Vec<String>> {
        self.run(inputs, lang)
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
        fn lines(&mut self, inputs: &[String], lang: Lang) -> Result<Vec<String>> {
            self.set_lang(lang)?;
            inputs.iter().map(|s| self.one_line(s)).collect()
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
    fn pedacos_da_palavra() {
        let p = |w: &str| word_pieces(w).into_iter().map(|(s, b)| (s, b)).collect::<Vec<_>>();
        assert_eq!(p("mundo."), vec![("mundo".into(), false), (".".into(), true)]);
        assert_eq!(
            p("“concerns—something”"),
            vec![
                ("“".into(), true),
                ("concerns".into(), false),
                ("—".into(), true),
                ("something".into(), false),
                ("”".into(), true)
            ]
        );
    }

    /// Backend falso que imita o espeak: fonemas = texto entre colchetes, funde
    /// "there were" numa palavra só e lê "2024" em três.
    struct Fake;
    impl Phonemizer for Fake {
        fn lines(&mut self, inputs: &[String], _lang: Lang) -> Result<Vec<String>> {
            Ok(inputs
                .iter()
                .map(|l| {
                    l.replace("there were", "therewere")
                        .replace("2024", "two twenty four")
                        .split_whitespace()
                        .map(|w| format!("[{w}]"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect())
        }
    }

    #[test]
    fn uma_entrada_por_palavra_mesmo_com_fusao_e_numeros() {
        let ph = Fake.phonemize("Olá, mundo. There were 2024 “risks—and” more?!", Lang::EnUs).unwrap();
        let pw = ph.per_word.unwrap();
        assert_eq!(
            pw,
            vec![
                "[Olá],",
                "[mundo].",
                "[Theɹe]",
                "[weɹe]",
                "[two] [twenty] [fouɹ]",
                "“[ɹisks]—[and]”",
                "[moɹe]?!",
            ]
        );
        assert_eq!(ph.phonemes, pw.join(" "));
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
        // "there were" é o caso que o espeak funde; tem que sair em duas palavras
        let m = p.phonemize("and there were some (schisms)", Lang::EnUs).unwrap();
        assert_eq!(m.per_word.as_ref().map(|w| w.len()), Some(5), "{m:?}");
        assert!(m.per_word.unwrap().iter().all(|w| !w.contains(' ')), "{}", m.phonemes);
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
