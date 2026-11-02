(() => {
  const text = (document.body?.innerText || '').toLowerCase();
  if (/доступ ограничен|подтвердите, что вы не робот|проверка браузера|слишком много запросов|captcha|access denied/.test(text + ' ' + document.title.toLowerCase()) || document.querySelector('iframe[src*="captcha"], [data-marker="captcha"]')) {
    return { state: 'blocked', items: [], next: null };
  }
  if (/\/(login|registration)(\/|\?|$)/.test(location.pathname)) {
    return { state: 'login', items: [], next: null };
  }
  const root = document.querySelector('[data-marker="catalog-serp"]') || document.querySelector('main') || document;
  const cards = [...root.querySelectorAll('[data-marker="item"]')];
  const items = cards.map(card => {
    const link = card.querySelector('a[data-marker="item-title"]') || card.querySelector('a[itemprop="url"]');
    if (!link) return null;
    const url = new URL(link.href, location.href);
    const id = url.pathname.match(/_(\d+)$/)?.[1];
    if (!id || !['www.avito.ru', 'avito.ru'].includes(url.hostname)) return null;
    url.search = ''; url.hash = '';
    const title = (link.querySelector('[itemprop="name"], h3')?.textContent || link.textContent || '').trim();
    const meta = card.querySelector('[itemprop="price"]');
    const raw = meta?.getAttribute('content') || card.querySelector('[data-marker="item-price"]')?.textContent || '';
    const digits = raw.replace(/[^0-9]/g, '');
    const price = /бесплатно/i.test(raw) ? 0 : digits ? Number(digits) : null;
    const locationText = card.querySelector('[data-marker="item-address"], [data-marker="item-location"], [class*="geo-root"]')?.textContent || '';
    return title ? { id, title, url: url.href, price: Number.isSafeInteger(price) ? price : null, location: locationText.trim() } : null;
  }).filter(Boolean);
  const next = document.querySelector('a[data-marker="pagination-button/next"]');
  const empty = /ничего не найдено|не нашли ничего|нет объявлений по вашему запросу/.test(text);
  return { state: items.length ? 'ready' : empty ? 'empty' : 'loading', items, next: next?.getAttribute('aria-disabled') === 'true' ? null : next?.href || null };
})()
