//! Heurística de agrupar linhas (saída de PDF) em parágrafos.
//!
//! Regras, na ordem em que a v1 em JS as aplicava:
//!   * linha vazia fecha o parágrafo em aberto;
//!   * linha terminando em hífen é emendada com a próxima sem espaço;
//!   * linha que termina em pontuação de fim (.!?:;»"”)) ou é "curta" fecha o parágrafo;
//!   * o resto acumula com espaço.
//! "Curta" é relativa à mediana das linhas da página, não um número fixo — em PDF
//! com fonte grande 40 chars era uma linha normal e a v1 quebrava tudo.

const CLOSERS: &[char] = &['.', '!', '?', ':', ';', '»', '"', '”', ')', '…'];

pub fn group_lines<'a, I>(lines: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let cleaned: Vec<String> = lines
        .into_iter()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();

    let short_limit = short_threshold(&cleaned);

    let mut out: Vec<String> = Vec::new();
    let mut acc = String::new();

    for line in &cleaned {
        if line.is_empty() {
            flush(&mut acc, &mut out);
            continue;
        }
        if acc.is_empty() {
            acc.push_str(line);
        } else if acc.ends_with('-') && !acc.ends_with(" -") {
            // hífen de quebra de linha: "traba-" + "lho" → "trabalho"
            acc.pop();
            acc.push_str(line);
        } else {
            acc.push(' ');
            acc.push_str(line);
        }
        let ends_sentence = line
            .chars()
            .last()
            .map(|c| CLOSERS.contains(&c))
            .unwrap_or(false);
        let is_short = line.chars().count() < short_limit;
        if ends_sentence || is_short {
            flush(&mut acc, &mut out);
        }
    }
    flush(&mut acc, &mut out);
    out
}

fn flush(acc: &mut String, out: &mut Vec<String>) {
    let t = acc.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    acc.clear();
}

/// Limiar de "linha curta": 60% da mediana do comprimento das linhas não vazias,
/// com piso de 25 e teto de 50 (o teto evita que tabelas/poemas virem um bloco só).
fn short_threshold(lines: &[String]) -> usize {
    let mut lens: Vec<usize> = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().count())
        .collect();
    if lens.is_empty() {
        return 40;
    }
    lens.sort_unstable();
    let median = lens[lens.len() / 2];
    ((median as f64) * 0.6).round().clamp(25.0, 50.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junta_linhas_e_fecha_em_pontuacao() {
        let lines = [
            "Era uma vez um leitor que gostava de livros",
            "longos e de café. Ele lia todos os dias.",
            "No dia seguinte, o leitor voltou.",
        ];
        let p = group_lines(lines);
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0],
            "Era uma vez um leitor que gostava de livros longos e de café. Ele lia todos os dias."
        );
        assert_eq!(p[1], "No dia seguinte, o leitor voltou.");
    }

    #[test]
    fn hifen_no_fim_emenda_sem_espaco() {
        let lines = [
            "Este é um texto com uma palavra hifeni-",
            "zada no meio da frase que continua aqui.",
        ];
        let p = group_lines(lines);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("hifenizada"));
    }

    #[test]
    fn linha_curta_e_linha_vazia_fecham() {
        let lines = [
            "Capítulo 1",
            "",
            "Uma linha razoavelmente longa que não termina em pontuação",
            "e outra linha razoavelmente longa que também não termina",
            "fim",
        ];
        let p = group_lines(lines);
        assert_eq!(p[0], "Capítulo 1");
        assert_eq!(p.len(), 2);
        assert!(p[1].ends_with("termina fim"));
    }

    #[test]
    fn colapsa_espacos_internos() {
        let p = group_lines(["a   b\t c."]);
        assert_eq!(p, vec!["a b c."]);
    }
}
