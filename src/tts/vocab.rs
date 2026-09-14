//! Mapa símbolo IPA → token id do Kokoro-82M (v1.0).
//!
//! É exatamente o `dict(zip(symbols, range(len(symbols))))` do hexgrad/Kokoro:
//! pad + pontuação + letras + IPA. Ordem importa e não pode mudar — os ids são os
//! embeddings do modelo. Repare que `'` aparece duas vezes na string original
//! (em volta do U+0329); em Python "o último vence", e fazemos igual aqui.

use std::collections::HashMap;
use std::sync::OnceLock;

const PAD: &str = "$";
const PUNCTUATION: &str = ";:,.!?¡¿—…\"«»“” ";
const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const LETTERS_IPA: &str = "ɑɐɒæɓʙβɔɕçɗɖðʤəɘɚɛɜɝɞɟʄɡɠɢʛɦɧħɥʜɨɪʝɭɬɫɮʟɱɯɰŋɳɲɴøɵɸθœɶʘɹɺɾɻʀʁɽʂʃʈʧʉʊʋⱱʌɣɤʍχʎʏʑʐʒʔʡʕʢǀǁǂǃˈˌːˑʼʴʰʱʲʷˠˤ˞↓↑→↗↘'̩'ᵻ";

/// Kokoro aceita no máximo 510 tokens entre os dois pads (contexto 512).
pub const MAX_TOKENS: usize = 510;

pub fn vocab() -> &'static HashMap<char, i64> {
    static V: OnceLock<HashMap<char, i64>> = OnceLock::new();
    V.get_or_init(|| {
        let symbols: String = [PAD, PUNCTUATION, LETTERS, LETTERS_IPA].concat();
        let mut m = HashMap::new();
        for (i, c) in symbols.chars().enumerate() {
            m.insert(c, i as i64); // último vence, como no dict() do Python
        }
        m
    })
}

/// Fonemas → ids, ignorando símbolos fora do vocabulário (igual ao Kokoro),
/// truncando em MAX_TOKENS. Sem os pads — `kokoro.rs` adiciona o 0 nas pontas.
pub fn tokenize(phonemes: &str) -> Vec<i64> {
    let v = vocab();
    phonemes
        .chars()
        .filter_map(|c| v.get(&c).copied())
        .take(MAX_TOKENS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_conhecidos() {
        // Valores batem com o tokenizer de referência (kokoro-onnx / Kokoros).
        let v = vocab();
        assert_eq!(v[&'$'], 0);
        assert_eq!(v[&';'], 1);
        assert_eq!(v[&' '], 16);
        assert_eq!(v[&'A'], 17);
        assert_eq!(v[&'a'], 43);
        assert_eq!(v[&'ɑ'], 69);
        assert_eq!(v[&'ˈ'], 156);
    }

    #[test]
    fn tokenize_descarta_desconhecidos() {
        let t = tokenize("hˈɛlə|oʊ");
        assert!(!t.contains(&-1));
        assert_eq!(t.len(), 7); // '|' some
    }
}
