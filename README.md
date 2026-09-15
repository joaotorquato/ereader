# reader

Leitor de PDF/EPUB no navegador (PWA) com leitura em voz alta pelo **Kokoro-82M**
rodando dentro do próprio binário (ONNX Runtime via `ort`), destaque da palavra
falada e posição lembrada por livro. Um executável, SQLite, sem Python.

```
┌───────────── iPhone / tablet (Safari, PWA) ─────────────┐
│ app.js: lista de chunks → GET /clips/{key} → <audio>    │
│ rAF sincroniza highlight com word_timings                │
└──────────────────────────┬───────────────────────────────┘
                           │ Tailscale
┌──────────────────────────▼───────────────────────────────┐
│ reader (axum)                                            │
│  extract::{pdf,epub} ─► SQLite (books/sections/paragraphs)│
│  /clips ─► tts::queue (1 worker, spawn_blocking)         │
│              espeak-ng ─► vocab ─► ort/Kokoro ─► WAV     │
│              timing (durations reais ou proporcional)    │
│              cache em clips + data/audio/xx/<sha>.wav    │
└──────────────────────────────────────────────────────────┘
```

## Requisitos

* Rust estável (1.80+).
* `espeak-ng` instalado no host (`apt install espeak-ng`). É o fonemizador; o
  binário chama `espeak-ng --ipa` por chunk. (Alternativa: feature `espeak-ffi`
  linka `libespeak-ng` — precisa de `libespeak-ng-dev`; ver nota abaixo.)
* `calibre` instalado no host (`ebook-convert` no PATH): todo PDF é convertido pra
  EPUB antes de extrair (título/capítulos/h1-h6 de verdade em vez de "Página N").
  Se o binário não existir, cai pro pdf-extract direto de sempre — não trava o upload.
* Recomendado: `poppler-utils` (`apt install poppler-utils` / `brew install poppler`).
  Usado no fallback direto (quando a conversão pra EPUB falha) em vez do
  `pdf-extract` puro-Rust, que engole glifos de ligadura sem ToUnicode (ex.: "Th")
  em alguns PDFs e é pior em PDF de duas colunas. Detectado e usado automaticamente
  se `pdftotext` estiver no PATH; passe `--pdftotext` só pra forçar outro caminho.
* Opcional: `epubcheck` (`--epubcheck /caminho/epubcheck`) pra validar o EPUB gerado.
  Se reprovar, tenta de novo com outra versão de EPUB (2 → 3) antes de desistir e cair
  pro fallback direto; sem `epubcheck` configurado, o EPUB da primeira conversão é
  usado sem checar.
* Modelo + vozes: `./scripts/download-models.sh` (≈ 92 MB + 0,5 MB por voz).

## Build local (x86_64)

```sh
./scripts/download-models.sh            # → ./models
cargo build --release                   # ort baixa o ONNX Runtime pré-compilado no build
READER_TOKEN=troque-isto ./target/release/reader --data-dir ./data
# abre http://localhost:8080/?token=troque-isto
```

Testes (os de fonemização só rodam se `espeak-ng` existir no PATH):

```sh
cargo test
```

## Cross-compile para Raspberry Pi 5 (aarch64)

O jeito que não dói é `cross` (Docker/Podman), porque o `rusqlite` bundled
compila C e o `ort` precisa do runtime certo:

```sh
cargo install cross
cross build --release --target aarch64-unknown-linux-gnu
scp target/aarch64-unknown-linux-gnu/release/reader pi:/usr/local/bin/
```

O que muda entre os alvos:

| | x86_64 | Pi 5 (aarch64) |
|---|---|---|
| ONNX Runtime | `ort` baixa binário com AVX2 | `ort` baixa binário aarch64 (NEON) |
| `--threads` | 2–4 | **2** (4 cores, mas deixe folga; int8 escala mal) |
| Modelo | qualquer variante | `quantized` (int8); fp32 fica ~3× mais lento |
| Latência por chunk (~200 chars) | ~0,3–0,6 s | ~1,5–3 s (por isso o prefetch de 3) |
| RAM residente | ~250 MB | ~250 MB (modelo int8 + runtime) |

Se o `ort` reclamar que não há binário para o alvo, use `ort = { features = ["load-dynamic"] }`
e aponte `ORT_DYLIB_PATH` para um `libonnxruntime.so` baixado do release oficial
(microsoft/onnxruntime tem `onnxruntime-linux-aarch64-*.tgz`).

Sem Docker, dá para compilar direto no Pi (`cargo build --release` leva uns
10–15 min na primeira vez; depois é incremental).

## Subir no servidor

```sh
sudo useradd -r -s /usr/sbin/nologin -d /var/lib/reader reader
sudo mkdir -p /var/lib/reader/{data,models}
sudo ./scripts/download-models.sh /var/lib/reader/models
sudo chown -R reader: /var/lib/reader
sudo cp target/release/reader /usr/local/bin/reader
echo "READER_TOKEN=$(openssl rand -hex 24)" | sudo tee /etc/reader.env; sudo chmod 600 /etc/reader.env
sudo cp deploy/reader.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now reader
journalctl -u reader -f
```

No celular: `http://<host-tailscale>:8080/?token=<o token>` — o app grava o token e
o cookie; depois "Adicionar à Tela de Início" para virar PWA.

## Configuração

Toda flag tem ENV equivalente (`reader --help`):

| flag | env | default |
|---|---|---|
| `--bind` | `READER_BIND` | `0.0.0.0:8080` |
| `--data-dir` | `READER_DATA_DIR` | `./data` |
| `--token` | `READER_TOKEN` | (obrigatório, ≥ 8 chars) |
| `--model` | `READER_MODEL` | `./models/kokoro-v1.0.onnx` |
| `--voices` | `READER_VOICES` | `./models/voices` (dir de `.bin`) ou um NPZ |
| `--threads` | `READER_THREADS` | `2` |
| `--voice-en` / `--voice-pt` | `READER_VOICE_EN` / `_PT` | `af_heart` / `pf_dora` |
| `--espeak-bin` | `READER_ESPEAK_BIN` | `espeak-ng` |
| `--pdftotext` | `READER_PDFTOTEXT` | detecta `pdftotext` no PATH; senão usa `pdf-extract` puro-Rust |
| `--ebook-convert` | `READER_EBOOK_CONVERT` | `ebook-convert` |
| `--epubcheck` | `READER_EPUBCHECK` | (desligado) |
| `--prefetch` | `READER_PREFETCH` | `4` |

## API (para debugar com curl)

```
GET  /books                            POST /books (multipart: file)
GET  /books/{id}                       DELETE /books/{id}
GET  /books/{id}/sections/{n}          PUT /books/{id}/position {section, paragraph}
GET  /clips?paragraph=ID[&voice=&speed=]          → chunks + clips já prontos
GET  /clips/{key}?paragraph=ID&chunk=N[&voice=&speed=] → espera gerar, devolve meta
GET  /audio/{aa}/{key}.wav             GET /voices          GET /health
```

Token: header `Authorization: Bearer`, cookie `reader_token` ou `?token=`.

## Notas de implementação

* **Fonemização.** `espeak-ng --ipa` descarta pontuação; quebramos o texto nas
  pontuações, fonemizamos cada pedaço como uma linha via stdin (um processo por
  chunk) e reinserimos a pontuação — é o que a lib `phonemizer` do Kokoro faz. Em
  cima disso aplicamos os ajustes do `kokoro-onnx` (`r→ɹ`, `x→k`, etc.).
* **Vozes.** Raw f32 `[510,1,256]` por voz (onnx-community) ou o NPZ do
  kokoro-onnx; o parser de `.npy` é caseiro (30 linhas) para não puxar ndarray.
* **Timings.** Com o modelo *timestamped* (default do script) usamos `durations`
  por token → alinhamento real por palavra (`clips.aligned = 1`). Sem isso, cai
  para proporcional por fonemas ou por chars. Tudo em `tts/timing.rs`.
* **Offsets em UTF-16**, não em bytes/chars, porque é o que `String.slice` do JS
  usa — emoji num livro não desalinha o marcador.
* **Fila.** Um worker (`spawn_blocking` + `mpsc`). Pedidos reais incrementam uma
  `epoch`; prefetches de epochs antigas sem ninguém esperando são descartados, então
  pular de parágrafo não deixa 3 inferências inúteis na frente do que você quer ouvir.
* **WAV 16-bit**, 24 kHz. `TODO(opus)` em `tts/kokoro.rs` se o tráfego pesar.
* **`espeak-ffi`** está escrita mas não foi compilada aqui (sem `libespeak-ng` no
  ambiente de build). Confira as assinaturas contra `speak_lib.h` antes de usar.

## Estado

Este código foi escrito sem acesso ao crates.io no ambiente de geração, ou seja,
**ainda não passou por `cargo build`**. Os pontos com maior chance de erro de
compilação estão comentados nos arquivos: `tts/kokoro.rs` (API do `ort` rc.11),
`extract/pdf.rs` (assinatura do `pdf-extract` 0.12) e `http/assets.rs`
(`IntoResponse` para `Vec<(HeaderName, String)>`). Rode `cargo build` e mande os
erros — o resto é ajuste de assinatura, não de arquitetura.
