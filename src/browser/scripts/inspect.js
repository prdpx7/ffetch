(() => {
  const snapshot = SNAPSHOT;
  const limit = LIMIT;
  const candidates = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,summary,[role="button"],[role="link"],[contenteditable="true"]')];
  const visible = candidates.filter(el => el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden');
  const nodes = new Map();
  const elements = visible.slice(0, limit).map((el, i) => {
    const ref = `@${snapshot}:e${i+1}`;
    nodes.set(ref, el);
    const labelled = (el.getAttribute('aria-labelledby') || '').split(/\s+/).map(id => document.getElementById(id)?.textContent || '').join(' ').trim();
    const label = el.getAttribute('aria-label') || labelled || [...(el.labels || [])].map(l => l.textContent).join(' ') || el.innerText || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || '';
    return {ref, tag:el.tagName.toLowerCase(), role:el.getAttribute('role'), type:el.getAttribute('type'), name:label.trim().slice(0,1000), href:el.href || null, disabled:!!el.disabled, required:!!el.required};
  });
  globalThis.__ffetchInspection = {snapshot, nodes, document};
  return {snapshot, title:document.title, url:location.href, elements, total_elements:visible.length, truncated:visible.length > limit, text:document.body?.innerText.slice(0,12000) || ''};
})()
