// Favor service worker: shows push notifications and opens the right tab.
// It caches nothing, so the app is always fresh from the server.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));

self.addEventListener('push', e => {
  let d = {};
  try { d = e.data ? e.data.json() : {}; } catch (err) { d = { title: e.data && e.data.text() }; }
  e.waitUntil((async () => {
    // Open pages refresh their bell straight away.
    const open = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    open.forEach(c => c.postMessage({ type: 'push' }));
    await self.registration.showNotification(d.title || 'Favor', {
      body: d.body || '',
      tag: d.tag,
      icon: '/icon-192.png',
      badge: '/icon-192.png',
      data: { link: d.link || 'today' },
    });
  })());
});

self.addEventListener('notificationclick', e => {
  e.notification.close();
  const go = (e.notification.data || {}).link || 'today';
  e.waitUntil((async () => {
    const open = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const page = open.find(c => new URL(c.url).origin === self.location.origin);
    if (page) {
      page.postMessage({ type: 'open', go });
      return page.focus();
    }
    return self.clients.openWindow('/?go=' + encodeURIComponent(go));
  })());
});
