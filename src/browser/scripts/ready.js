new Promise((resolve, reject) => {
  const selector = WAIT_FOR;
  let observer;
  let timer;
  const cleanup = () => {
    observer?.disconnect();
    clearTimeout(timer);
    document.removeEventListener('readystatechange', check);
    window.removeEventListener('load', check, true);
    document.removeEventListener('input', check, true);
    document.removeEventListener('change', check, true);
  };
  const check = () => {
    try {
      const ready = document.readyState !== 'loading' && (selector
        ? !!document.querySelector(selector)
        : !!document.body && document.body.innerText.trim().length > 0);
      if (ready) {
        cleanup();
        resolve(true);
      }
    } catch (error) {
      cleanup();
      reject(error);
    }
  };
  observer = new MutationObserver(check);
  observer.observe(document, {childList: true, subtree: true, characterData: true, attributes: true});
  document.addEventListener('readystatechange', check);
  window.addEventListener('load', check, true);
  document.addEventListener('input', check, true);
  document.addEventListener('change', check, true);
  timer = setTimeout(() => {
    cleanup();
    reject(new Error('browser readiness deadline exceeded'));
  }, TIMEOUT_MS);
  check();
})
