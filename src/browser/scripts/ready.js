new Promise((resolve, reject) => {
  const selector = WAIT_FOR;
  const challenge = CHALLENGE_RULES;
  let challengeSeen = false;
  let challengeActive = false;
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
      const title = document.title.trim().toLowerCase();
      if (challenge.denied_titles.includes(title) || document.querySelector(challenge.denied_selector)) {
        cleanup();
        reject(new Error('browser_challenge_denied: access denied by the site'));
        return;
      }
      challengeActive = challenge.titles.includes(title) || !!document.querySelector(challenge.selector);
      if (challengeActive) {
        challengeSeen = true;
        return;
      }
      const ready = document.readyState !== 'loading' && (selector
        ? !!document.querySelector(selector)
        : !!document.body && document.body.innerText.trim().length > 0);
      if (ready) {
        cleanup();
        resolve({challenge_waited: challengeSeen});
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
    reject(new Error(challengeActive
      ? 'browser_challenge_unresolved: interstitial did not clear before the deadline'
      : 'browser readiness deadline exceeded'));
  }, TIMEOUT_MS);
  check();
})
