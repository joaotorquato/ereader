//! Parágrafo → chunks de ≤ MAX_CHARS, sempre em fim de frase: um parágrafo que
//! cabe vira um chunk só; senão, o máximo de frases inteiras por chunk. Uma frase
//! só é cortada no meio (vírgula/espaço) se sozinha já passa de MAX_CHARS — o
//! Kokoro aceita no máximo 510 tokens de fonema e estouraria.
//!
//! Offsets são em unidades UTF-16, não em bytes nem em `char`: é o que o JS usa
//! em `String.prototype.slice`/`length`, e o front precisa cortar o parágrafo nos
//! mesmos pontos para pintar a palavra certa. Para texto latino é igual a chars;
//! para emoji/astrais difere — e livro tem emoji hoje em dia.

use serde::Serialize;

/// ~400 chars ≈ 400 fonemas+acentos, com folga para o teto de 510 tokens do modelo.
/// ponytail: teto em chars, não em tokens; medir fonemas se alguma língua estourar.
pub const MAX_CHARS: usize = 400;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Chunk {
    pub idx: usize,
    /// Offset UTF-16 do início do chunk dentro do parágrafo.
    pub offset: usize,
    pub text: String,
}

pub fn split(paragraph: &str) -> Vec<Chunk> {
    let sentences = sentences(paragraph);
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur = String::new();
    let mut cur_off = 0usize;
    let mut off = 0usize;

    let push = |cur: &mut String, cur_off: usize, out: &mut Vec<Chunk>| {
        let t = cur.trim_end();
        if !t.is_empty() {
            out.push(Chunk {
                idx: out.len(),
                offset: cur_off,
                text: t.to_string(),
            });
        }
        cur.clear();
    };

    for s in sentences {
        let s_len = s.chars().count();
        if s_len > MAX_CHARS {
            // frase gigante: fecha o que tinha e quebra a frase em pedaços por vírgula/espaço
            push(&mut cur, cur_off, &mut out);
            let mut piece_off = off;
            for piece in split_long(&s) {
                let trimmed_lead = piece.len() - piece.trim_start().len();
                let lead16 = piece[..trimmed_lead].encode_utf16().count();
                let mut c = piece.trim_start().to_string();
                c = c.trim_end().to_string();
                if !c.is_empty() {
                    out.push(Chunk {
                        idx: out.len(),
                        offset: piece_off + lead16,
                        text: c,
                    });
                }
                piece_off += piece.encode_utf16().count();
            }
            off += s.encode_utf16().count();
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + s_len > MAX_CHARS {
            push(&mut cur, cur_off, &mut out);
        }
        if cur.is_empty() {
            // pula espaços iniciais da frase para o offset apontar na 1ª letra
            let lead = s.len() - s.trim_start().len();
            cur_off = off + s[..lead].encode_utf16().count();
            cur.push_str(s.trim_start());
        } else {
            cur.push_str(&s);
        }
        off += s.encode_utf16().count();
    }
    push(&mut cur, cur_off, &mut out);
    if out.is_empty() && !paragraph.trim().is_empty() {
        out.push(Chunk {
            idx: 0,
            offset: 0,
            text: paragraph.trim().to_string(),
        });
    }
    out
}

/// Divide em frases mantendo a pontuação e o espaço que segue colados à frase
/// (assim a soma dos pedaços reconstrói o texto e os offsets fecham).
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?' | '…') {
            // engole pontuação repetida e aspas de fechamento
            while let Some(&n) = chars.peek() {
                if matches!(n, '.' | '!' | '?' | '…' | '"' | '”' | '’' | '»' | ')' | '\'') {
                    cur.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            // só é fim de frase se vier espaço (ou fim); "3.14" e "e.g." não quebram
            match chars.peek() {
                Some(&n) if n.is_whitespace() => {
                    while let Some(&n) = chars.peek() {
                        if n.is_whitespace() {
                            cur.push(n);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    out.push(std::mem::take(&mut cur));
                }
                None => out.push(std::mem::take(&mut cur)),
                _ => {}
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Frase maior que MAX_CHARS: enche até o limite e corta na última pontuação
/// fraca (, ; : —) que coube; sem nenhuma (ou cedo demais), no último espaço.
fn split_long(s: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_inclusive(' ') {
        if cur.chars().count() + word.chars().count() > MAX_CHARS && !cur.is_empty() {
            let cut = cur
                .trim_end()
                .rfind([',', ';', ':', '—', '–'])
                .map(|i| i + cur[i..].chars().next().unwrap().len_utf8())
                .filter(|&i| cur[..i].chars().count() > MAX_CHARS / 4);
            let rest = cut.map(|i| cur.split_off(i)).unwrap_or_default();
            pieces.push(std::mem::replace(&mut cur, rest));
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrupa_frases_ate_o_limite() {
        let p = "Primeira frase. Segunda frase! Terceira?";
        let c = split(p);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, p);
        assert_eq!(c[0].offset, 0);
    }

    #[test]
    fn quebra_em_fim_de_frase_e_offsets_fecham() {
        let a = "a".repeat(250) + ". ";
        let b = "b".repeat(250) + ".";
        let p = format!("{a}{b}");
        let c = split(&p);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].offset, 0);
        assert_eq!(c[1].offset, 252);
        let js_like: Vec<u16> = p.encode_utf16().collect();
        let slice = String::from_utf16(
            &js_like[c[1].offset..c[1].offset + c[1].text.encode_utf16().count()],
        )
        .unwrap();
        assert_eq!(slice, c[1].text);
    }

    #[test]
    fn offset_em_utf16_com_emoji() {
        let p = "Olá 😀 mundo. Segunda frase bem longa ".to_string() + &"x".repeat(370) + ".";
        let c = split(&p);
        assert_eq!(c.len(), 2, "{c:?}");
        // "Olá 😀 mundo. " tem 13 chars mas 14 unidades UTF-16
        assert_eq!(c[1].offset, 14);
    }

    #[test]
    fn frase_gigante_e_cortada() {
        let p = "palavra ".repeat(80); // 640 chars sem pontuação
        let c = split(&p);
        assert!(c.len() >= 2);
        assert!(c.iter().all(|c| c.text.chars().count() <= MAX_CHARS));
    }

    #[test]
    fn frase_gigante_corta_na_ultima_virgula_que_coube() {
        // vírgulas em ~150 e ~350; o corte deve ser na de ~350, não na primeira
        let p = format!(
            "{}, {}, {}.",
            "aa ".repeat(50).trim_end(),
            "bb ".repeat(66).trim_end(),
            "cc ".repeat(100).trim_end()
        );
        let c = split(&p);
        assert!(c[0].text.ends_with("bb,"), "{:?}", c[0].text);
        assert!(c[0].text.chars().count() > 300);
        assert_eq!(c[1].offset, c[0].text.encode_utf16().count() + 1);
        let all: String = c.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(all, p);
    }

    #[test]
    fn nunca_corta_frase_que_cabe() {
        // frase longa sem pontuação interna: antes viraria 2 chunks; agora fica inteira
        let s = "palavra ".repeat(40).trim_end().to_string() + "."; // ~320 chars
        let p = format!("Curta. {s} Outra curta.");
        let c = split(&p);
        assert!(c.iter().any(|c| c.text.contains(&s)), "{c:?}");
        assert!(c.iter().all(|c| c.text.ends_with('.')), "{c:?}");
    }

    #[test]
    fn paragrafo_que_cabe_vira_um_chunk() {
        let p = "Frase um. Frase dois, com vírgula! Frase três? ".repeat(6);
        let c = split(&p);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].text, p.trim());
    }

    #[test]
    fn nao_quebra_em_ponto_decimal() {
        let s = sentences("Pi é 3.14 e ponto. Fim.");
        assert_eq!(s, vec!["Pi é 3.14 e ponto. ", "Fim."]);
    }
}
