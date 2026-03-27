from __future__ import annotations

import shutil
import subprocess
from typing import TYPE_CHECKING

from ffetch.tiers.base import BaseFetcher
from ffetch.types import FetchResult, FetchStatus, Tier

if TYPE_CHECKING:
    from ffetch.config import Config


class RenderFetcher(BaseFetcher):
    def _lp_bin(self) -> str:
        name = self.config.defaults.lightpanda_bin if self.config else "lightpanda"
        return shutil.which(name) or ""

    def available(self) -> bool:
        if self._lp_bin():
            return True
        try:
            import playwright  # noqa: F401
            return True
        except ImportError:
            return False

    def fetch(self, url: str, timeout: int = 30) -> FetchResult:
        lp = self._lp_bin()
        if lp:
            result = self._lightpanda_fetch(lp, url, timeout)
            if result.ok:
                return result
        return self._playwright_fallback(url, timeout)

    def _lightpanda_fetch(self, lp_bin: str, url: str, timeout: int) -> FetchResult:
        try:
            proc = subprocess.run(
                [lp_bin, "fetch", "--dump", "html", url],
                capture_output=True,
                text=True,
                timeout=timeout,
            )
            html = proc.stdout.strip()
            if not html or "<html" not in html.lower()[:500]:
                return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.ERROR, error="lightpanda returned no html")

            return FetchResult(
                url=url,
                tier=Tier.RENDER,
                status=FetchStatus.OK,
                html=html,
                status_code=200,
            )
        except subprocess.TimeoutExpired:
            return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.ERROR, error="lightpanda timed out")
        except Exception as e:
            return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.ERROR, error=str(e))

    def _playwright_fallback(self, url: str, timeout: int) -> FetchResult:
        try:
            from playwright.sync_api import sync_playwright
        except ImportError:
            return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.ERROR, error="neither lightpanda nor playwright available")

        defaults = self.config.defaults if self.config else None
        wait = defaults.playwright_wait if defaults else "networkidle"
        sleep_sec = defaults.playwright_sleep if defaults else 1.0

        try:
            with sync_playwright() as p:
                browser = p.chromium.launch(headless=True)
                page = browser.new_page()
                page.goto(url, timeout=timeout * 1000, wait_until=wait)
                if sleep_sec > 0:
                    page.wait_for_timeout(int(sleep_sec * 1000))
                html = page.content()
                browser.close()

            return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.OK, html=html, status_code=200)
        except Exception as e:
            return FetchResult(url=url, tier=Tier.RENDER, status=FetchStatus.ERROR, error=str(e))
