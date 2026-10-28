(() => {
  const ref = REFERENCE;
  const state = globalThis.__ffetchInspection;
  if (!state || state.document !== document || !state.nodes.has(ref)) throw new Error('stale_reference: run browser inspect again');
  const el = state.nodes.get(ref);
  if (!el.isConnected || !el.getClientRects().length) throw new Error('stale_reference: element is detached or hidden; inspect again');
  if (el.disabled || el.getAttribute('aria-disabled') === 'true') throw new Error('element_disabled');
  el.scrollIntoView({block:'center',inline:'center'});
  ACTION
})()
