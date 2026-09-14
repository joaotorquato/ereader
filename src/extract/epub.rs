//! EPUB → uma seção por item do spine.
//!
//! Pipeline: zip → META-INF/container.xml (roxmltree) → OPF (roxmltree: metadata,
//! manifest, spine) → cada XHTML do spine (scraper/html5ever, tolerante a XHTML
//! malformado, que é a regra em EPUB de loja).
//!
//! roxmltree em vez de um parser XML "de verdade" com namespaces porque só precisamos
//! de local names (`rootfile`, `item`, `itemref`, `title`, `language`) — e ele
//! compara por local name quando você passa `&str` para `has_tag_name`.

use super::{Extracted, Paragraph, Section};
use anyhow::{anyhow, bail, Context, Result};
use scraper::{ElementRef, Html, Selector};
use std::io::{Cursor, Read};
use zip::ZipArchive;

const BLOCK_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6", "p", "li", "blockquote"];
const SKIP_TAGS: &[&str] = &["script", "style", "nav", "svg", "math", "figure", "table"];

pub fn extract(bytes: &[u8]) -> Result<Extracted> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).context("abrindo zip do EPUB")?;

    let container = read_string(&mut zip, "META-INF/container.xml")?;
    let opf_path = {
        let doc = roxmltree::Document::parse(&container).context("container.xml")?;
        doc.descendants()
            .find(|n| n.has_tag_name("rootfile"))
            .and_then(|n| n.attribute("full-path"))
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("container.xml sem rootfile"))?
    };
    let base = match opf_path.rfind('/') {
        Some(i) => opf_path[..=i].to_string(),
        None => String::new(),
    };

    let opf_xml = read_string(&mut zip, &opf_path)?;
    let opf = roxmltree::Document::parse(&opf_xml).context("OPF")?;

    let meta_text = |name: &str| -> String {
        opf.descendants()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let title = meta_text("title");
    let lang = meta_text("language");

    // manifest: id → href (relativo ao OPF)
    let mut manifest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in opf.descendants().filter(|n| n.has_tag_name("item")) {
        if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
            manifest.insert(id.to_string(), href.to_string());
        }
    }

    let mut sections = Vec::new();
    for itemref in opf.descendants().filter(|n| n.has_tag_name("itemref")) {
        let Some(idref) = itemref.attribute("idref") else {
            continue;
        };
        let Some(href) = manifest.get(idref) else {
            continue;
        };
        let href = href.split('#').next().unwrap_or(href);
        let path = normalize_path(&format!("{base}{}", percent_decode(href)));
        let Ok(html) = read_string(&mut zip, &path) else {
            tracing::warn!(path = %path, "item do spine não encontrado no zip");
            continue;
        };
        if let Some(sec) = section_from_xhtml(&html, sections.len() + 1) {
            sections.push(sec);
        }
    }
    if sections.is_empty() {
        bail!("EPUB sem texto legível no spine");
    }
    Ok(Extracted {
        title,
        lang,
        sections,
    })
}

fn section_from_xhtml(html: &str, ordinal: usize) -> Option<Section> {
    let doc = Html::parse_document(html);
    let block_sel = Selector::parse(&BLOCK_TAGS.join(",")).unwrap();

    let mut paragraphs = Vec::new();
    let mut title = String::new();

    for el in doc.select(&block_sel) {
        if has_skip_ancestor(el) {
            continue;
        }
        // Se este bloco contém outro bloco (li > p, blockquote > p), deixa os filhos
        // falarem por ele — senão o texto sairia duplicado.
        if contains_block(el, &block_sel) {
            continue;
        }
        let text = el.text().collect::<String>();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let kind = el.value().name().to_ascii_lowercase();
        if kind.starts_with('h') && title.is_empty() {
            title = text.clone();
        }
        paragraphs.push(Paragraph { kind, text });
    }

    if paragraphs.is_empty() {
        return None;
    }
    if title.is_empty() {
        title = doc
            .select(&Selector::parse("title").unwrap())
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Seção {ordinal}"));
    }
    Some(Section { title, paragraphs })
}

fn has_skip_ancestor(el: ElementRef) -> bool {
    el.ancestors()
        .filter_map(ElementRef::wrap)
        .any(|a| SKIP_TAGS.contains(&a.value().name()))
}

/// `ElementRef::select` inclui ou não o próprio elemento dependendo da versão do
/// scraper; comparando ids evitamos depender disso.
fn contains_block(el: ElementRef, sel: &Selector) -> bool {
    el.select(sel).any(|d| d.id() != el.id())
}

fn read_string(zip: &mut ZipArchive<Cursor<&[u8]>>, path: &str) -> Result<String> {
    let mut f = zip
        .by_name(path)
        .with_context(|| format!("{path} não existe no EPUB"))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .with_context(|| format!("lendo {path}"))?;
    Ok(s)
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Resolve "OEBPS/../x.xhtml" e "./x" sem tocar no filesystem.
fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn tiny_epub() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let o = SimpleFileOptions::default();
            w.start_file("mimetype", o).unwrap();
            w.write_all(b"application/epub+zip").unwrap();
            w.start_file("META-INF/container.xml", o).unwrap();
            w.write_all(r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.as_bytes()).unwrap();
            w.start_file("OEBPS/content.opf", o).unwrap();
            w.write_all(r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Livro de Teste</dc:title><dc:language>pt-BR</dc:language></metadata><manifest><item id="c1" href="cap%201.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="cap2.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#.as_bytes()).unwrap();
            w.start_file("OEBPS/cap 1.xhtml", o).unwrap();
            w.write_all(r#"<html><head><title>x</title></head><body><h1>Capítulo   Um</h1><p>Primeiro <em>parágrafo</em>.</p><ul><li><p>Item com p dentro.</p></li></ul><script>ignorar()</script></body></html>"#.as_bytes()).unwrap();
            w.start_file("OEBPS/cap2.xhtml", o).unwrap();
            w.write_all(
                r#"<html><body><blockquote>Uma citação.</blockquote></body></html>"#.as_bytes(),
            )
            .unwrap();
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extrai_metadata_e_secoes() {
        let ex = extract(&tiny_epub()).unwrap();
        assert_eq!(ex.title, "Livro de Teste");
        assert_eq!(ex.lang, "pt-BR");
        assert_eq!(ex.sections.len(), 2);
        let s0 = &ex.sections[0];
        assert_eq!(s0.title, "Capítulo Um");
        let texts: Vec<&str> = s0.paragraphs.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Capítulo Um", "Primeiro parágrafo.", "Item com p dentro."]
        );
        assert_eq!(s0.paragraphs[0].kind, "h1");
        assert_eq!(s0.paragraphs[2].kind, "p");
        assert_eq!(ex.sections[1].paragraphs[0].kind, "blockquote");
        assert_eq!(ex.sections[1].title, "Seção 2");
    }

    #[test]
    fn normaliza_caminhos() {
        assert_eq!(normalize_path("OEBPS/../text/./a.xhtml"), "text/a.xhtml");
        assert_eq!(normalize_path("OEBPS/a.xhtml"), "OEBPS/a.xhtml");
    }
}
