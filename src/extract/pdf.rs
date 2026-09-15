//! PDF → EPUB (Calibre `ebook-convert`) → `epub::extract`, que dá título/capítulos/
//! h1-h6 de verdade — é o que habilita reflow (leitura dinâmica) em vez de "Página N"
//! fixa. Se `epubcheck` está configurado e reprova a conversão, tenta de novo com
//! outra versão de EPUB antes de desistir. Se nada disso rolar (ferramenta ausente,
//! PDF que trava o conversor, EPUB sempre inválido), cai pro pipeline direto de
//! sempre: `pdf-extract` (puro Rust) com fallback pra `pdftotext` (poppler) por
//! subprocesso — melhor em PDF de duas colunas e em fontes sem ToUnicode.

use super::{paragraphs, Extracted, ExtractOptions, Paragraph, Section};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn extract(bytes: &[u8], opts: &ExtractOptions) -> Result<Extracted> {
    match pdf_to_epub(bytes, opts) {
        Ok(epub_bytes) => match super::epub::extract(&epub_bytes) {
            Ok(ex) => return Ok(ex),
            Err(e) => {
                tracing::warn!(error = %e, "EPUB convertido sem texto legível; caindo pro pdf-extract direto")
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "conversão PDF→EPUB falhou; caindo pro pdf-extract direto")
        }
    }
    extract_direct(bytes, opts.pdftotext)
}

/// Pipeline direto de sempre (sem passar por EPUB): uma seção por página, parágrafos
/// por heurística de linhas. Usado só quando a conversão pra EPUB não deu certo.
pub fn extract_direct(bytes: &[u8], pdftotext: Option<&Path>) -> Result<Extracted> {
    let pages: Vec<String> = match pdftotext {
        Some(bin) => pages_via_pdftotext(bin, bytes)?,
        None => pages_via_pdf_extract(bytes)?,
    };
    Ok(Extracted {
        title: String::new(),
        lang: String::new(),
        sections: pages_to_sections(&pages),
    })
}

/// Versões de EPUB tentadas em ordem quando o epubcheck reprova a conversão — "3" tem
/// mais recurso mas o epubcheck é mais rígido com ele; "2" é o mais tolerante.
const EPUB_VERSIONS: &[&str] = &["2", "3"];

/// Converte por arquivo temporário: nem `ebook-convert` nem `epubcheck` leem
/// stdin/stdout como o `pdftotext` acima. O sufixo (pid + contador) evita colisão
/// entre uploads concorrentes; `TempFiles` limpa os dois arquivos ao sair, sucesso
/// ou erro. Se `epubcheck` está configurado e reprova a primeira versão, tenta de
/// novo com a próxima antes de desistir (sem epubcheck, a primeira já basta —
/// não tem o que ajustar sem validação pra guiar).
fn pdf_to_epub(bytes: &[u8], opts: &ExtractOptions) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir();
    let stem = format!("reader-pdf2epub-{}-{}", std::process::id(), unique_suffix());
    let pdf_path = dir.join(format!("{stem}.pdf"));
    let epub_path = dir.join(format!("{stem}.epub"));
    let _cleanup = TempFiles(&[&pdf_path, &epub_path]);
    std::fs::write(&pdf_path, bytes).context("gravando PDF temporário")?;

    let mut last_err = None;
    for version in EPUB_VERSIONS {
        let attempt = run_converter(
            opts.ebook_convert,
            &["--enable-heuristics", "--filter-css", "--epub-version", version],
            &pdf_path,
            &epub_path,
        )
        .and_then(|_| validate_epub(&epub_path, opts.epubcheck));
        match attempt {
            Ok(()) => return std::fs::read(&epub_path).context("lendo EPUB convertido"),
            Err(e) => {
                tracing::warn!(error = %e, version, "conversão PDF→EPUB falhou; ajustando versão do EPUB");
                let _ = std::fs::remove_file(&epub_path);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("EPUB_VERSIONS não é vazio"))
}

fn run_converter(bin: &str, extra_args: &[&str], input: &Path, output: &Path) -> Result<()> {
    let out = Command::new(bin)
        .arg(input)
        .arg(output)
        .args(extra_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("executando {bin}"))?;
    if !out.status.success() {
        bail!(
            "{bin} saiu com {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `epubcheck` sai com 0 só quando não há erro (avisos não fazem o exit code falhar).
fn validate_epub(path: &Path, epubcheck: Option<&Path>) -> Result<()> {
    let Some(bin) = epubcheck else {
        return Ok(());
    };
    let out = Command::new(bin)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("executando {}", bin.display()))?;
    if !out.status.success() {
        bail!(
            "epubcheck reportou erros: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

// ponytail: pid+contador+nanos em vez da crate `tempfile` — só precisamos de um nome
// que não colida entre requisições concorrentes, não de um diretório isolado de verdade.
fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_mul(1_000_003).wrapping_add(n)
}

struct TempFiles<'a>(&'a [&'a Path]);
impl Drop for TempFiles<'_> {
    fn drop(&mut self) {
        for p in self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn pages_via_pdf_extract(bytes: &[u8]) -> Result<Vec<String>> {
    // pdf-extract 0.12 expõe `extract_text_from_mem_by_pages(&[u8]) -> Result<Vec<String>, OutputError>`.
    // O panic hook abaixo existe porque a crate ainda dá panic em alguns PDFs malformados
    // (fontes Type3, streams corrompidos); catch_unwind vira isso num erro HTTP 422
    // em vez de derrubar o worker.
    let res = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes));
    match res {
        Ok(Ok(pages)) => Ok(pages),
        Ok(Err(e)) => bail!("pdf-extract: {e}"),
        Err(_) => bail!("pdf-extract entrou em panic neste PDF; tente com --pdftotext"),
    }
}

fn pages_via_pdftotext(bin: &Path, bytes: &[u8]) -> Result<Vec<String>> {
    // `pdftotext - -` lê da stdin e escreve na stdout; páginas separadas por \f
    // (não passe -nopgbrk: é justamente o \f que usamos para separar páginas).
    let mut child = Command::new(bin)
        .args(["-enc", "UTF-8", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("executando {}", bin.display()))?;
    {
        let mut stdin = child.stdin.take().context("stdin do pdftotext")?;
        stdin.write_all(bytes)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("pdftotext saiu com {}", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.split('\x0c').map(|s| s.to_string()).collect())
}

/// Glifos sem `ToUnicode` (ligaduras, fontes Type3/corrompidas) saem do
/// `pdf-extract`/`pdftotext` como caracteres de controle — na prática quase
/// sempre U+0000. Isso não só suja o texto: um NUL no meio da linha faz o
/// espeak-ng (lê stdin como string C) truncar ali, o parágrafo inteiro fica
/// sem fonemas e a síntese falha. Troca por espaço (absorvido no colapso de
/// espaços do `group_lines`) em vez de apagar, pra não colar duas palavras.
fn strip_bad_chars(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
        .collect()
}

fn pages_to_sections(pages: &[String]) -> Vec<Section> {
    let mut sections = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let page = strip_bad_chars(page);
        let paras = paragraphs::group_lines(page.lines());
        if paras.is_empty() {
            continue; // página só com imagem/vazia: não vira seção
        }
        sections.push(Section {
            title: format!("Página {}", i + 1),
            paragraphs: paras
                .into_iter()
                .map(|text| Paragraph {
                    kind: "p".into(),
                    text,
                })
                .collect(),
        });
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PDF mínimo, duas páginas, texto em Helvetica (WinAnsi). Montado à mão para
    /// não precisar de fixture binária no repo; o xref é recalculado em `build_pdf`.
    fn tiny_pdf() -> Vec<u8> {
        let page_text = [
            "BT /F1 12 Tf 50 700 Td (Hello reader, this is page one.) Tj ET",
            "BT /F1 12 Tf 50 700 Td (Second page starts here.) Tj ET",
        ];
        let mut objs: Vec<String> = Vec::new();
        objs.push("<< /Type /Catalog /Pages 2 0 R >>".into());
        objs.push("<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into());
        for (i, t) in page_text.iter().enumerate() {
            let content_id = 4 + i * 2;
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {content_id} 0 R /Resources << /Font << /F1 7 0 R >> >> >>"
            ));
            objs.push(format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                t.len(),
                t
            ));
        }
        objs.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .into(),
        );
        build_pdf(&objs)
    }

    fn build_pdf(objs: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, o).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                objs.len() + 1,
                xref
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn extrai_uma_secao_por_pagina() {
        let ex = extract_direct(&tiny_pdf(), None).expect("pdf-extract");
        check_tiny_pdf_sections(&ex);
    }

    #[test]
    fn ebook_convert_ausente_cai_pro_pdf_extract_direto() {
        // Cenário real desta máquina: calibre não instalado. `extract()` deve tentar a
        // conversão, falhar (ENOENT), e ainda assim devolver texto via extract_direct.
        let opts = ExtractOptions {
            pdftotext: None,
            ebook_convert: "reader-test-ebook-convert-inexistente",
            epubcheck: None,
        };
        let ex = extract(&tiny_pdf(), &opts).expect("fallback pro pdf-extract direto");
        check_tiny_pdf_sections(&ex);
    }

    fn check_tiny_pdf_sections(ex: &Extracted) {
        assert_eq!(ex.sections.len(), 2);
        assert_eq!(ex.sections[0].title, "Página 1");
        assert!(
            ex.sections[0].paragraphs[0].text.contains("page one"),
            "{:?}",
            ex.sections[0]
        );
        assert!(ex.sections[1].paragraphs[0].text.contains("Second page"));
    }

    #[test]
    fn nul_de_ligadura_sem_tounicode_vira_espaco_sem_colar_palavras() {
        // exatamente o padrão visto em produção: glifo de ligadura ("Th") sem
        // ToUnicode saiu como NUL do pdf-extract.
        let page = "\u{0}is book isn't about music history.\nSecond\u{1}line.";
        let clean = strip_bad_chars(page);
        assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
        assert_eq!(clean.lines().count(), 2, "não pode fundir linhas");
        assert_eq!(clean.lines().next().unwrap(), " is book isn't about music history.");
        assert_eq!(clean.lines().nth(1).unwrap(), "Second line.", "não pode colar as palavras");
    }

    #[test]
    fn detecta_kind_por_magic() {
        assert_eq!(
            super::super::detect_kind("x.bin", b"%PDF-1.7"),
            Some(super::super::Kind::Pdf)
        );
        assert_eq!(
            super::super::detect_kind("x.epub", b"PK\x03\x04"),
            Some(super::super::Kind::Epub)
        );
        assert_eq!(super::super::detect_kind("x.txt", b"hello"), None);
    }
}
