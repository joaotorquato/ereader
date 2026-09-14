// Service worker: cacheia só o shell (HTML/CSS/JS/manifest/ícone).
// API e áudio vão sempre à rede — o servidor é local e o cache de clips é dele.
const SHELL = "reader-shell-v1";
const FILES = ["/", "/app.js", "/style.css", "/manifest.webmanifest", "/icon.svg"];

self.addEventListener("install", e => {
  e.waitUntil(caches.open(SHELL).then(c => c.addAll(FILES)).then(() => self.skipWaiting()));
});
self.addEventListener("activate", e => {
  e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k !== SHELL).map(k => caches.delete(k)))).then(() => self.clients.claim()));
});
self.addEventListener("fetch", e => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin) return;
  if (!FILES.includes(url.pathname)) return; // API/áudio: rede
  // network-first para o shell (é local, rápido; e sempre pega a versão nova),
  // cache como fallback offline
  e.respondWith(
    fetch(e.request).then(r => { const copy = r.clone(); caches.open(SHELL).then(c => c.put(e.request, copy)); return r; })
      .catch(() => caches.match(e.request))
  );
});
