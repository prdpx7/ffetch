from __future__ import annotations

from ffetch.tiers.base import BaseFetcher
from ffetch.types import FetchResult, FetchStatus, Tier


class StealthFetcher(BaseFetcher):
    def available(self) -> bool:
        try:
            import botasaurus  # noqa: F401
            return True
        except ImportError:
            return False

    def fetch(self, url: str, timeout: int = 30) -> FetchResult:
        defaults = self.config.defaults if self.config else None
        sleep_sec = defaults.stealth_sleep if defaults else 2.0

        try:
            from botasaurus.browser import browser, Driver

            html_result = []

            @browser(headless=True, block_images=True)
            def scrape(driver: Driver, data):
                driver.get(url)
                driver.sleep(sleep_sec)
                html_result.append(driver.page_html)

            scrape()
            html = html_result[0] if html_result else ""

            return FetchResult(
                url=url,
                tier=Tier.STEALTH,
                status=FetchStatus.OK if html else FetchStatus.ERROR,
                html=html,
                status_code=200 if html else 0,
            )
        except ImportError:
            return FetchResult(
                url=url,
                tier=Tier.STEALTH,
                status=FetchStatus.ERROR,
                error="botasaurus not installed. Install with: uv pip install ffetch[stealth]",
            )
        except Exception as e:
            return FetchResult(url=url, tier=Tier.STEALTH, status=FetchStatus.ERROR, error=str(e))
