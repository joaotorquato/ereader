/* Reader — front sem framework, sem build.
 *
 * Fluxo de leitura:
 *   tocar num parágrafo → GET /clips?paragraph=ID (lista de chunks + o que já está
 *   pronto) → para cada chunk: GET /clips/{key}?paragraph&chunk (espera gerar) →
 *   baixa o WAV como blob → toca no <audio> único → rAF sincroniza o highlight
 *   com audio.currentTime usando word_timings (offsets UTF-16 dentro do chunk,
 *   somados ao offset do chunk no parágrafo = índice UTF-16 no texto do parágrafo,
 *   que é o que String.slice usa aqui).
 *   Enquanto toca o chunk N, já pede o N+1 (o servidor também pré-gera N+1..N+3).
 *   Fim do parágrafo → próximo; fim da seção → carrega a próxima e segue.
 *
 * Um único <audio> porque o iOS só deixa tocar áudio iniciado por gesto do usuário;
 * trocar `src` e chamar play() dentro do handler de `ended` do mesmo elemento
 * mantém a "permissão".
 */
(() => {
"use strict";
const $ = id => document.getElementById(id);

// ---------------------------------------------------------------- token / api
const TOKEN_KEY = "reader_token";
let token = localStorage.getItem(TOKEN_KEY) || "";
{ // /?token=xxx no primeiro acesso
  const u = new URL(location.href);
  const t = u.searchParams.get("token");
  if (t) { token = t; localStorage.setItem(TOKEN_KEY, t); u.searchParams.delete("token"); history.replaceState(null, "", u.pathname); }
}
function setCookie() {
  // o <audio> e o SW não mandam header; cookie resolve
  document.cookie = `reader_token=${encodeURIComponent(token)}; path=/; max-age=31536000; SameSite=Lax`;
}
setCookie();

async function api(path, opts = {}) {
  const headers = Object.assign({ Authorization: "Bearer " + token }, opts.headers || {});
  const r = await fetch(path, Object.assign({}, opts, { headers }));
  if (r.status === 401) { await askToken(); return api(path, opts); }
  if (!r.ok) {
    let msg = r.statusText;
    try { msg = (await r.json()).error || msg; } catch {}
    const e = new Error(msg); e.status = r.status; throw e;
  }
  return r.status === 204 ? null : r.json();
}

function askToken() {
  return new Promise(resolve => {
    const d = $("token-dialog");
    d.querySelector("form").onsubmit = () => {
      token = $("token-input").value.trim();
      localStorage.setItem(TOKEN_KEY, token); setCookie(); resolve();
    };
    if (!d.open) d.showModal();
  });
}

// ------------------------------------------------------------------ theme
const root = document.documentElement;
function applyTheme(t) { if (t) root.dataset.theme = t; else delete root.dataset.theme; localStorage.setItem("theme", t || ""); }
applyTheme(localStorage.getItem("theme") || "");
$("theme").onclick = () => {
  const cur = root.dataset.theme || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  applyTheme(cur === "dark" ? "light" : "dark");
};

// -------------------------------------------------------------- settings/TOC
const settingsDialog = $("settings-dialog");
$("settings").onclick = () => { renderToc(); settingsDialog.showModal(); };
settingsDialog.addEventListener("click", e => { if (e.target === settingsDialog) settingsDialog.close(); });

let tocCache = new Map(); // idx da seção → sub-títulos (h2..h6) já carregados
async function headingsFor(idx) {
  if (tocCache.has(idx)) return tocCache.get(idx);
  const s = await api(`/books/${st.book.id}/sections/${idx}`);
  // idx 0 é o próprio título do capítulo (já alcançável clicando nele); o resto são sub-seções
  const heads = s.paragraphs.filter(p => p.idx > 0 && /^h[1-6]$/.test(p.kind));
  tocCache.set(idx, heads);
  return heads;
}

function goToToc(idx, paraIdx = 0) {
  settingsDialog.close();
  stop();
  loadSection(idx, paraIdx);
}

async function toggleChapter(body, caret, idx, forceOpen) {
  const open = forceOpen || body.hidden;
  body.hidden = !open;
  caret.textContent = open ? "⌄" : "›";
  if (!open || body.dataset.loaded) return;
  body.dataset.loaded = "1";
  body.textContent = "Carregando…";
  try {
    const heads = await headingsFor(idx);
    body.textContent = "";
    if (!heads.length) {
      const e = document.createElement("small"); e.className = "toc-empty"; e.textContent = "Sem sub-seções";
      body.appendChild(e);
    }
    for (const h of heads) {
      const b = document.createElement("button");
      b.className = "toc-heading " + h.kind;
      b.textContent = h.text;
      b.onclick = () => goToToc(idx, h.idx);
      body.appendChild(b);
    }
  } catch {
    body.textContent = ""; body.dataset.loaded = "";
    const e = document.createElement("small"); e.className = "toc-empty"; e.textContent = "Erro ao carregar";
    body.appendChild(e);
  }
}

function renderToc() {
  const box = $("toc");
  box.innerHTML = "";
  if (!st.book) { box.innerHTML = `<p class="toc-empty">Abra um livro na biblioteca.</p>`; return; }
  let activeChap = null;
  st.book.sections.forEach((sec, idx) => {
    const chap = document.createElement("div");
    chap.className = "toc-chapter" + (idx === st.sec ? " active" : "");

    const head = document.createElement("div"); head.className = "toc-chapter-head";
    const titleBtn = document.createElement("button"); titleBtn.className = "toc-title-btn";
    titleBtn.textContent = `${idx + 1}. ${sec.title}`;
    titleBtn.onclick = () => goToToc(idx);
    const caret = document.createElement("button"); caret.className = "toc-caret"; caret.setAttribute("aria-label", "Expandir");
    caret.textContent = "›";
    head.append(titleBtn, caret);

    const body = document.createElement("div"); body.className = "toc-sections"; body.hidden = true;
    caret.onclick = () => toggleChapter(body, caret, idx);

    chap.append(head, body);
    box.appendChild(chap);
    if (idx === st.sec) activeChap = { chap, body, caret };
  });
  // sempre mostra onde o usuário está agora; role a lista pra cima pra ver os demais capítulos
  if (activeChap) {
    toggleChapter(activeChap.body, activeChap.caret, st.sec, true);
    requestAnimationFrame(() => activeChap.chap.scrollIntoView({ block: "center" }));
  }
}

// ------------------------------------------------------------------ state
const st = {
  book: null,          // {id, title, lang, sections:[{id,idx,title}], pos_section, pos_paragraph}
  sec: 0,              // índice da seção carregada
  paras: [],           // parágrafos da seção: [{id, idx, kind, text}]
  para: -1,            // índice do parágrafo tocando
  playing: false,
  gen: 0,              // "geração" do playback: incrementa em stop(); callbacks antigos checam e saem
  tts: { available: false, voices: [], default: {} },
  webSpeech: false,    // fallback ativo
};
const audio = $("audio");
const rate = () => +$("rate").value;
const voice = () => $("voice").value || "";

// -------------------------------------------------------------- library
async function showLibrary() {
  stop();
  st.book = null;
  $("reader").hidden = true; $("player").hidden = true; $("progress").hidden = true; $("back").hidden = true;
  $("library").hidden = false;
  $("title").textContent = "Reader"; $("where").textContent = "";
  const books = await api("/books");
  const ul = $("books"); ul.innerHTML = "";
  $("library-empty").hidden = books.length > 0;
  for (const b of books) {
    const li = document.createElement("li");
    const t = document.createElement("div"); t.className = "t";
    t.innerHTML = `<b></b><small></small>`;
    t.querySelector("b").textContent = b.title;
    t.querySelector("small").textContent = `${b.kind.toUpperCase()} · ${b.section_count} seções · ${b.lang || "idioma?"} · seção ${b.pos_section + 1}`;
    t.onclick = () => openBook(b.id);
    const del = document.createElement("button"); del.className = "del"; del.textContent = "remover";
    del.onclick = async () => { if (confirm(`Remover "${b.title}"?`)) { await api(`/books/${b.id}`, { method: "DELETE" }); showLibrary(); } };
    li.append(t, del); ul.appendChild(li);
  }
}

$("file").onchange = async e => {
  const f = e.target.files[0]; if (!f) return;
  const fd = new FormData(); fd.append("file", f, f.name);
  $("upload-status").textContent = `Enviando ${f.name}…`;
  try {
    const r = await api("/books", { method: "POST", body: fd });
    $("upload-status").textContent = "";
    openBook(r.id);
  } catch (err) { $("upload-status").textContent = "Falhou: " + err.message; }
  e.target.value = "";
};

// --------------------------------------------------------------- reader
async function openBook(id) {
  stop();
  st.book = await api(`/books/${id}`);
  tocCache = new Map();
  $("library").hidden = true; $("reader").hidden = false; $("player").hidden = false; $("progress").hidden = false; $("back").hidden = false;
  $("title").textContent = st.book.title;
  await fillVoices();
  await loadSection(st.book.pos_section, st.book.pos_paragraph);
}
$("back").onclick = showLibrary;

async function loadSection(idx, scrollToPara = 0) {
  idx = Math.max(0, Math.min(idx, st.book.sections.length - 1));
  const s = await api(`/books/${st.book.id}/sections/${idx}`);
  st.sec = idx; st.paras = s.paragraphs; st.para = -1;
  $("where").textContent = `${idx + 1} / ${st.book.sections.length}`;
  $("bar").style.width = ((idx + 1) / st.book.sections.length * 100) + "%";
  $("prev-section").disabled = idx === 0;
  $("next-section").disabled = idx >= st.book.sections.length - 1;
  renderParagraphs();
  const target = st.paras.find(p => p.idx === scrollToPara);
  if (target && scrollToPara > 0) requestAnimationFrame(() => paraEl(target.idx)?.scrollIntoView({ block: "start" }));
  else $("main").scrollTop = 0;
}

function renderParagraphs() {
  const box = $("paragraphs"); box.innerHTML = "";
  for (const p of st.paras) {
    const tag = p.kind === "blockquote" ? "blockquote" : "p";
    const el = document.createElement(tag);
    el.className = "para " + (p.kind.startsWith("h") ? p.kind : "");
    el.dataset.p = p.idx;
    // spans por palavra, com o offset UTF-16 (= índice JS) de cada uma
    let pos = 0;
    for (const tok of p.text.split(/(\s+)/)) {
      if (!tok) continue;
      if (/^\s+$/.test(tok)) el.appendChild(document.createTextNode(tok));
      else { const w = document.createElement("span"); w.className = "w"; w.dataset.c = pos; w.textContent = tok; el.appendChild(w); }
      pos += tok.length;
    }
    el.onclick = () => { stop(); st.para = p.idx; play(); };
    box.appendChild(el);
  }
}
const paraEl = idx => document.querySelector(`.para[data-p="${idx}"]`);

$("prev-section").onclick = () => { stop(); loadSection(st.sec - 1); };
$("next-section").onclick = () => { stop(); loadSection(st.sec + 1); };

// touch: os botões de seção somem (CSS, pointer:coarse); toque na borda ou
// arrasto horizontal troca de seção em qualquer altura da página.
if (matchMedia("(pointer: coarse)").matches) {
  const goSection = d => {
    if (!st.book || $("reader").hidden) return false;
    const target = st.sec + d;
    if (target < 0 || target >= st.book.sections.length) return false;
    stop(); loadSection(target);
    return true;
  };
  let tx = 0, ty = 0, tt = 0;
  $("main").addEventListener("touchstart", e => {
    const t = e.touches[0]; tx = t.clientX; ty = t.clientY; tt = Date.now();
  }, { passive: true });
  $("main").addEventListener("touchend", e => {
    const t = e.changedTouches[0];
    const dx = t.clientX - tx, dy = t.clientY - ty;
    const edge = Math.min(72, innerWidth * 0.15);
    let moved;
    if (Math.abs(dx) > 70 && Math.abs(dx) > Math.abs(dy) * 1.5) moved = goSection(dx < 0 ? 1 : -1);
    else if (Date.now() - tt < 500 && Math.abs(dx) < 10 && Math.abs(dy) < 10) {
      if (t.clientX < edge) moved = goSection(-1);
      else if (t.clientX > innerWidth - edge) moved = goSection(1);
    }
    if (moved) e.preventDefault();
  });
}

let posTimer = null;
function savePosition() {
  clearTimeout(posTimer);
  posTimer = setTimeout(() => {
    if (st.book) api(`/books/${st.book.id}/position`, {
      method: "PUT", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ section: st.sec, paragraph: Math.max(0, st.para) }),
    }).catch(() => {});
  }, 800);
}

// ------------------------------------------------------------ highlight
let hl = { pi: -1, el: null };
function highlightWord(pi, charIdx) {
  const p = paraEl(pi); if (!p) return;
  if (hl.pi !== pi) {
    document.querySelectorAll(".para.reading").forEach(e => e.classList.remove("reading"));
    p.classList.add("reading"); hl.pi = pi;
  }
  let hit = null;
  for (const w of p.querySelectorAll(".w")) { if (+w.dataset.c <= charIdx) hit = w; else break; }
  if (hit && hit !== hl.el) {
    hl.el?.classList.remove("on"); hit.classList.add("on"); hl.el = hit;
    const r = hit.getBoundingClientRect(), m = $("main").getBoundingClientRect();
    if (r.top < m.top + 60 || r.bottom > m.bottom - 120) hit.scrollIntoView({ block: "center", behavior: "smooth" });
  }
}
function clearHighlight() {
  hl.el?.classList.remove("on"); hl = { pi: -1, el: null };
  document.querySelectorAll(".para.reading").forEach(e => e.classList.remove("reading"));
}

// --------------------------------------------------------------- player
function setStatus(t) { $("status").textContent = t; }
function setPlayingUI(on) {
  st.playing = on;
  $("play").textContent = on ? "❚❚" : "▶";
  $("play").setAttribute("aria-label", on ? "Pausar" : "Tocar");
  if ("mediaSession" in navigator) navigator.mediaSession.playbackState = on ? "playing" : "paused";
}

function play() {
  if (!st.book || !st.paras.length) return;
  if (st.para < 0) st.para = 0;
  const g = ++st.gen;
  setPlayingUI(true);
  savePosition();
  if (st.webSpeech) speakParaWeb(g); else speakPara(g);
}

function stop() {
  st.gen++;
  setPlayingUI(false);
  audio.pause(); audio.removeAttribute("src"); audio.load();
  if (window.speechSynthesis) speechSynthesis.cancel();
  clearHighlight(); setStatus("");
}

function nextParagraph(g) {
  if (g !== st.gen) return;
  st.para++;
  if (st.para >= st.paras.length) {
    if (st.sec < st.book.sections.length - 1) {
      loadSection(st.sec + 1).then(() => { if (g !== st.gen) return; st.para = 0; savePosition(); st.webSpeech ? speakParaWeb(g) : speakPara(g); });
    } else { stop(); setStatus("Fim do livro"); }
    return;
  }
  savePosition();
  st.webSpeech ? speakParaWeb(g) : speakPara(g);
}

/** Lista de chunks + download em pipeline de um parágrafo. Guardado em
 *  st.prep pra começar o parágrafo seguinte enquanto o atual ainda toca
 *  (consumido uma vez em speakPara — os object URLs são revogados ao tocar). */
function prepPara(p) {
  const q = `voice=${encodeURIComponent(voice())}&speed=${rate()}`;
  if (st.prep && st.prep.id === p.id && st.prep.q === q) return st.prep.job;
  const job = (async () => {
    const list = await api(`/clips?paragraph=${p.id}&${q}`);
    const prepared = list.chunks.map(() => null);
    const prepare = async k => {
      if (prepared[k]) return prepared[k];
      prepared[k] = (async () => {
        const c = list.chunks[k];
        const meta = c.clip || await api(`/clips/${c.key}?paragraph=${p.id}&chunk=${c.idx}&${q}`);
        const blob = await (await fetch(meta.url, { headers: { Authorization: "Bearer " + token } })).blob();
        return { meta, url: URL.createObjectURL(blob), offset: c.offset };
      })();
      return prepared[k];
    };
    if (list.chunks.length) prepare(0).catch(() => {});
    return { list, prepare };
  })();
  st.prep = { id: p.id, q, job };
  return job;
}

/** Toca um parágrafo inteiro: pede a lista de chunks e encadeia. */
async function speakPara(g) {
  const p = st.paras[st.para];
  if (!p) return;
  const job = prepPara(p);
  st.prep = null;
  let list, prepare;
  try { ({ list, prepare } = await job); }
  catch (err) {
    if (err.status === 503 && "speechSynthesis" in window) {
      setStatus("TTS do servidor indisponível — usando voz do aparelho");
      st.webSpeech = true; return speakParaWeb(g);
    }
    setStatus("Erro: " + err.message); stop(); return;
  }
  if (g !== st.gen) return;
  if (!list.chunks.length) return nextParagraph(g);

  for (let k = 0; k < list.chunks.length; k++) {
    if (g !== st.gen) return;
    if (k === 0) setStatus("Gerando áudio…");
    let clip;
    try { clip = await prepare(k); } catch (err) { setStatus("Erro no áudio: " + err.message); stop(); return; }
    if (g !== st.gen) { URL.revokeObjectURL(clip.url); return; }
    setStatus("");
    // pipeline: chunk k+1 deste parágrafo, ou o parágrafo seguinte inteiro
    if (k + 1 < list.chunks.length) prepare(k + 1).catch(() => {});
    else if (st.paras[st.para + 1]) prepPara(st.paras[st.para + 1]).catch(() => {});
    await playClip(g, p.idx, clip);
    URL.revokeObjectURL(clip.url);
    if (g !== st.gen) return;
  }
  nextParagraph(g);
}

/** Toca um clip e resolve quando termina. Highlight via rAF. */
function playClip(g, pi, clip) {
  return new Promise(resolve => {
    const timings = clip.meta.word_timings || [];
    let raf = 0, i = 0;
    const tick = () => {
      if (g !== st.gen) return;
      const ms = audio.currentTime * 1000;
      while (i + 1 < timings.length && timings[i + 1].s <= ms) i++;
      while (i > 0 && timings[i].s > ms) i--;
      if (timings[i]) highlightWord(pi, clip.offset + timings[i].o);
      raf = requestAnimationFrame(tick);
    };
    const done = () => { cancelAnimationFrame(raf); audio.onended = audio.onerror = audio.onplaying = null; resolve(); };
    audio.onended = done;
    audio.onerror = () => { setStatus("Erro ao tocar o áudio"); done(); };
    audio.onplaying = () => { if (timings[0]) highlightWord(pi, clip.offset + timings[0].o); raf = requestAnimationFrame(tick); };
    audio.src = clip.url;
    audio.playbackRate = 1; // a velocidade já está no áudio gerado
    audio.play().catch(err => { setStatus("Toque em ▶ para ouvir"); setPlayingUI(false); done(); });
    updateMediaSession();
  });
}

// ---------------------------------------------------- Web Speech fallback
function speakParaWeb(g) {
  const p = st.paras[st.para]; if (!p || g !== st.gen) return;
  const u = new SpeechSynthesisUtterance(p.text);
  const want = (st.book.lang || "en").slice(0, 2).toLowerCase();
  const v = speechSynthesis.getVoices().find(v => v.lang.toLowerCase().startsWith(want));
  if (v) { u.voice = v; u.lang = v.lang; }
  u.rate = rate();
  u.onboundary = ev => { if (g === st.gen && (!ev.name || ev.name === "word")) highlightWord(p.idx, ev.charIndex); };
  u.onstart = () => highlightWord(p.idx, 0);
  u.onend = () => nextParagraph(g);
  u.onerror = ev => { if (ev.error !== "interrupted" && ev.error !== "canceled") { setStatus("Web Speech: " + ev.error); stop(); } };
  speechSynthesis.cancel(); speechSynthesis.speak(u);
}

// ---------------------------------------------------------- controls
$("play").onclick = () => st.playing ? stop() : play();
$("next").onclick = () => { stop(); st.para = Math.min(st.para + 1, st.paras.length - 1); play(); };
$("prev").onclick = () => { stop(); st.para = Math.max(0, st.para - 1); play(); };
$("rate").onchange = () => { if (st.playing) { const p = st.para; stop(); st.para = p; play(); } };
$("voice").onchange = () => { localStorage.setItem("voice:" + (st.book?.lang || ""), voice()); if (st.playing) { const p = st.para; stop(); st.para = p; play(); } };

async function fillVoices() {
  try { st.tts = await api("/voices"); } catch { st.tts = { available: false, voices: [], default: {} }; }
  const sel = $("voice"); sel.innerHTML = "";
  st.webSpeech = !st.tts.available;
  if (!st.tts.available) {
    const o = document.createElement("option"); o.value = ""; o.textContent = "voz do aparelho"; sel.appendChild(o); return;
  }
  const isPt = (st.book.lang || "").toLowerCase().startsWith("pt");
  const def = isPt ? st.tts.default.pt : st.tts.default.en;
  const saved = localStorage.getItem("voice:" + (st.book.lang || ""));
  // só vozes que sabemos fonemizar (a*/b* inglês, p* português), a padrão primeiro
  const usable = st.tts.voices.filter(v => /^[abp][fm]_/.test(v));
  usable.sort((a, b) => (a === def ? -1 : b === def ? 1 : a.localeCompare(b)));
  for (const v of usable) { const o = document.createElement("option"); o.value = v; o.textContent = v; sel.appendChild(o); }
  sel.value = usable.includes(saved) ? saved : def;
}

function updateMediaSession() {
  if (!("mediaSession" in navigator) || !st.book) return;
  navigator.mediaSession.metadata = new MediaMetadata({
    title: st.book.sections[st.sec]?.title || st.book.title,
    artist: st.book.title,
    album: "Reader",
  });
  const h = (a, f) => { try { navigator.mediaSession.setActionHandler(a, f); } catch {} };
  h("play", () => play()); h("pause", () => stop());
  h("nexttrack", () => $("next").onclick()); h("previoustrack", () => $("prev").onclick());
}

// --------------------------------------------------------------- boot
if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});
window.addEventListener("pagehide", () => { if (window.speechSynthesis) speechSynthesis.cancel(); });
showLibrary().catch(err => setStatus("Erro: " + err.message));
})();
