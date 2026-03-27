from __future__ import annotations

from typing import TYPE_CHECKING

from curl_cffi import requests as curl_requests

from ffetch.signals import blocked_signals, js_required_signals
from ffetch.tiers.base import BaseFetcher
from ffetch.types import FetchResult, FetchStatus, Tier

if TYPE_CHECKING:
    from ffetch.config import Config


class FastFetcher(BaseFetcher):
    def available(self) -> bool:
        return True

    def fetch(self, url: str, timeout: int = 30) -> FetchResult:
        defaults = self.config.defaults if self.config else None
        impersonate = defaults.impersonate if defaults else "chrome"

        try:
            resp = curl_requests.get(
                url,
                impersonate=impersonate,
                timeout=timeout,
                allow_redirects=True,
            )
            html = resp.text
            status = self._detect_status(html, resp.status_code)
            return FetchResult(
                url=url,
                tier=Tier.FAST,
                status=status,
                html=html,
                status_code=resp.status_code,
                headers=dict(resp.headers),
            )
        except Exception as e:
            return FetchResult(
                url=url,
                tier=Tier.FAST,
                status=FetchStatus.ERROR,
                error=str(e),
            )

    def _detect_status(self, html: str, status_code: int) -> FetchStatus:
        if status_code == 403:
            return FetchStatus.BLOCKED
        if status_code >= 400:
            return FetchStatus.ERROR

        defaults = self.config.defaults if self.config else None
        blocked = (defaults.blocked_signals if defaults and defaults.blocked_signals else None) or blocked_signals()
        js_signals = (defaults.js_required_signals if defaults and defaults.js_required_signals else None) or js_required_signals()
        threshold = defaults.html_size_threshold if defaults else 5000

        lower = html.lower()
        for signal in blocked:
            if signal in lower:
                return FetchStatus.BLOCKED
        for signal in js_signals:
            if signal in lower and len(html) < threshold:
                return FetchStatus.JS_REQUIRED

        return FetchStatus.OK
