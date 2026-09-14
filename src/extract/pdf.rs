//! PDF → uma seção por página, parágrafos por heurística de linhas.
//!
//! Primeira opção: `pdf-extract` (puro Rust). Fallback: `pdftotext` (poppler)
//! por subprocesso, ativado quando o caminho é passado (`--pdftotext`). O fallback
//! é bem melhor em PDF de duas colunas (`-layout` desligado deixa o poppler seguir
//! a ordem de leitura) e em PDF com fontes sem ToUnicode.

use super::{paragraphs, Extracted, Paragraph, Section};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn extract(bytes: &[u8], pdftotext: Option<&Path>) -> Result<Extracted> {
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

fn pages_to_sections(pages: &[String]) -> Vec<Section> {
    let mut sections = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
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
        let ex = extract(&tiny_pdf(), None).expect("pdf-extract");
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
