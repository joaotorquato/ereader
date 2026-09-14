//! Distribuição de tempo por palavra dentro de um chunk.
//!
//! Três níveis de qualidade, do melhor para o pior, escolhidos em `word_timings`:
//!   1. `durations` do modelo "timestamped" (um valor por token, incluindo os pads):
//!      somamos os frames dos tokens de cada palavra. Alinhamento real.
//!   2. fonemas por palavra (`per_word` do phonemizer): peso = nº de fonemas.
//!   3. só texto: peso = nº de chars da palavra (+1 por pontuação anexada).
//!
//! Tudo aqui é função pura sobre slices — fácil de testar e de trocar.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WordTiming {
    /// offset UTF-16 da palavra dentro do chunk
    pub o: usize,
    /// comprimento UTF-16
    pub l: usize,
    pub s: u32, // start ms
    pub e: u32, // end ms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Word<'a> {
    pub text: &'a str,
    pub offset16: usize,
    pub len16: usize,
}

/// Palavras = runs de não-espaço, com offsets UTF-16.
pub fn words(text: &str) -> Vec<Word<'_>> {
    let mut out = Vec::new();
    let mut off16 = 0usize;
    let mut start_byte: Option<usize> = None;
    let mut start16 = 0usize;
    for (i, c) in text.char_indices() {
        let is_space = c.is_whitespace();
        match (start_byte, is_space) {
            (None, false) => {
                start_byte = Some(i);
                start16 = off16;
            }
            (Some(sb), true) => {
                out.push(Word {
                    text: &text[sb..i],
                    offset16: start16,
                    len16: off16 - start16,
                });
                start_byte = None;
            }
            _ => {}
        }
        off16 += c.len_utf16();
    }
    if let Some(sb) = start_byte {
        out.push(Word {
            text: &text[sb..],
            offset16: start16,
            len16: off16 - start16,
        });
    }
    out
}

/// Entrada opcional do alinhamento real.
pub struct Durations<'a> {
    /// Um valor por token do modelo, na mesma ordem dos ids enviados (com os pads).
    pub per_token: &'a [f32],
    /// Ids enviados ao modelo (com pads), para saber onde caem os espaços.
    pub token_ids: &'a [i64],
    /// id do token "espaço" no vocab.
    pub space_id: i64,
}

pub fn word_timings(
    text: &str,
    duration_ms: u32,
    per_word_phonemes: Option<&[String]>,
    durations: Option<Durations>,
) -> Vec<WordTiming> {
    let ws = words(text);
    if ws.is_empty() || duration_ms == 0 {
        return vec![];
    }

    if let Some(d) = durations {
        if let Some(t) = from_durations(&ws, duration_ms, d) {
            return t;
        }
    }

    let weights: Vec<f32> = match per_word_phonemes {
        Some(p) if p.len() == ws.len() => p.iter().map(|ph| phoneme_weight(ph)).collect(),
        _ => ws.iter().map(|w| char_weight(w.text)).collect(),
    };
    proportional(&ws, duration_ms, &weights)
}

fn phoneme_weight(ph: &str) -> f32 {
    // marcas de tônica/longa não custam tempo próprio
    let n = ph
        .chars()
        .filter(|c| !matches!(c, 'ˈ' | 'ˌ' | 'ː' | 'ˑ'))
        .count();
    (n.max(1) as f32) + 0.5 // +0.5: fronteira de palavra tem um custo fixo
}

fn char_weight(w: &str) -> f32 {
    let letters = w.chars().filter(|c| c.is_alphanumeric()).count();
    let punct = w
        .chars()
        .filter(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '…'))
        .count();
    (letters.max(1) as f32) + 0.5 + (punct as f32) * 1.5 // pausa depois de pontuação
}

fn proportional(ws: &[Word], duration_ms: u32, weights: &[f32]) -> Vec<WordTiming> {
    let total: f32 = weights.iter().sum::<f32>().max(1e-6);
    let mut out = Vec::with_capacity(ws.len());
    let mut acc = 0f32;
    for (w, wt) in ws.iter().zip(weights) {
        let s = (acc / total * duration_ms as f32).round() as u32;
        acc += wt;
        let e = (acc / total * duration_ms as f32).round() as u32;
        out.push(WordTiming {
            o: w.offset16,
            l: w.len16,
            s,
            e: e.max(s),
        });
    }
    if let Some(last) = out.last_mut() {
        last.e = duration_ms;
    }
    out
}

/// Soma as durations por "palavra fonêmica" (tokens entre espaços, pulando pads
/// e pontuação solta). Se a contagem de palavras fonêmicas ≠ palavras do texto,
/// devolve None e o chamador cai para o proporcional.
fn from_durations(ws: &[Word], duration_ms: u32, d: Durations) -> Option<Vec<WordTiming>> {
    if d.per_token.len() != d.token_ids.len() || d.token_ids.len() < 3 {
        return None;
    }
    let punct_ids: Vec<i64> = {
        let v = super::vocab::vocab();
        ";:,.!?¡¿—…\"«»“”"
            .chars()
            .filter_map(|c| v.get(&c).copied())
            .collect()
    };
    // agrupa: cada grupo = frames dos tokens de uma palavra + frames da pontuação
    // e espaço que a seguem (a pausa "pertence" à palavra anterior, visualmente).
    let mut groups: Vec<f32> = Vec::new();
    let mut cur = 0f32;
    let mut in_word = false;
    let mut leading = 0f32; // pad inicial + silêncio antes da 1ª palavra
    let n = d.token_ids.len();
    for i in 0..n {
        let id = d.token_ids[i];
        let fr = d.per_token[i];
        let is_pad = i == 0 || i == n - 1;
        let is_space = id == d.space_id;
        let is_punct = punct_ids.contains(&id);
        if is_pad || is_space || is_punct {
            if in_word {
                cur += fr;
            } else if groups.is_empty() {
                leading += fr;
            } else if let Some(last) = groups.last_mut() {
                *last += fr;
            }
            if is_space && in_word {
                groups.push(cur);
                cur = 0.0;
                in_word = false;
            }
        } else {
            cur += fr;
            in_word = true;
        }
    }
    if in_word {
        groups.push(cur);
    }
    if groups.len() != ws.len() {
        return None;
    }
    let total_frames: f32 = leading + groups.iter().sum::<f32>();
    if total_frames <= 0.0 {
        return None;
    }
    let ms_per_frame = duration_ms as f32 / total_frames;
    let mut out = Vec::with_capacity(ws.len());
    let mut acc = leading;
    for (w, g) in ws.iter().zip(&groups) {
        let s = (acc * ms_per_frame).round() as u32;
        acc += g;
        let e = (acc * ms_per_frame).round() as u32;
        out.push(WordTiming {
            o: w.offset16,
            l: w.len16,
            s,
            e: e.max(s),
        });
    }
    if let Some(last) = out.last_mut() {
        last.e = duration_ms;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_com_offsets_utf16() {
        let w = words("Olá 😀 mundo!");
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].text, w[0].offset16, w[0].len16), ("Olá", 0, 3));
        assert_eq!((w[1].text, w[1].offset16, w[1].len16), ("😀", 4, 2));
        assert_eq!((w[2].text, w[2].offset16, w[2].len16), ("mundo!", 7, 6));
    }

    #[test]
    fn proporcional_por_chars_cobre_toda_a_duracao() {
        let t = word_timings("um dois três.", 1000, None, None);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].s, 0);
        assert_eq!(t[2].e, 1000);
        for pair in t.windows(2) {
            assert_eq!(pair[0].e, pair[1].s);
        }
        // "três." pesa mais que "um"
        assert!(t[2].e - t[2].s > t[0].e - t[0].s);
    }

    #[test]
    fn proporcional_por_fonemas_quando_alinha() {
        let ph = vec!["ˈa".to_string(), "bbbbbbbb".to_string()];
        let t = word_timings("a b", 900, Some(&ph), None);
        // pesos 1.5 e 8.5 → 135 ms e 765 ms
        assert_eq!(t[0].e, 135);
        assert_eq!(t[1].s, 135);
        assert_eq!(t[1].e, 900);
    }

    #[test]
    fn durations_reais_somam_por_palavra() {
        let v = super::super::vocab::vocab();
        let sp = v[&' '];
        let a = v[&'a'];
        let dot = v[&'.'];
        // [pad, a, a, sp, a, ., pad]
        let ids = [0, a, a, sp, a, dot, 0];
        let fr = [2.0, 10.0, 10.0, 4.0, 20.0, 6.0, 2.0];
        let d = Durations {
            per_token: &fr,
            token_ids: &ids,
            space_id: sp,
        };
        let t = word_timings("aa a.", 540, None, Some(d));
        // total 54 frames → 10 ms/frame; leading 2 → 1ª palavra 20..260 (a,a,space)
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].s, 20);
        assert_eq!(t[0].e, 260);
        assert_eq!(t[1].s, 260);
        assert_eq!(t[1].e, 540);
    }

    #[test]
    fn durations_incompativeis_caem_para_proporcional() {
        let ids = [0i64, 43, 0];
        let fr = [1.0f32, 1.0, 1.0];
        let d = Durations {
            per_token: &fr,
            token_ids: &ids,
            space_id: 16,
        };
        let t = word_timings("duas palavras", 100, None, Some(d));
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].e, 100);
    }
}
