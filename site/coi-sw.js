// Cross-origin-isolation service worker: GitHub Pages cannot set response
// headers, and SharedArrayBuffer (the Ctrl-C interrupt flag) requires
// COOP/COEP on the document. This worker stamps them onto every same-origin
// response; index.html registers it and reloads once so the *document*
// response gets the headers too. Everything the site loads is same-origin,
// so require-corp embeds nothing that would break.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (ev) => ev.waitUntil(self.clients.claim()));
self.addEventListener('fetch', (ev) => {
  const req = ev.request;
  if (req.cache === 'only-if-cached' && req.mode !== 'same-origin') return;
  ev.respondWith((async () => {
    const res = await fetch(req);
    if (res.status === 0) return res; // opaque — pass through untouched
    const headers = new Headers(res.headers);
    headers.set('Cross-Origin-Embedder-Policy', 'require-corp');
    headers.set('Cross-Origin-Opener-Policy', 'same-origin');
    return new Response(res.body, {
      status: res.status,
      statusText: res.statusText,
      headers,
    });
  })());
});
