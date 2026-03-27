from __future__ import annotations

import sys
import time

from ffetch.cache import FileCache
from ffetch.config import Config, ExtractionConfig, match_site, load_config
from ffetch.convert import html_to_markdown, html_to_text
from ffetch.tiers import FastFetcher, RenderFetcher, StealthFetcher
from ffetch.types import FetchResult, FetchStatus, Tier

TIER_MAP = {
    "fast": Tier.FAST,
    "render": Tier.RENDER,
    "stealth": Tier.STEALTH,
}


def _build_fetchers(config: Config) -> dict[Tier, FastFetcher | RenderFetcher | StealthFetcher]:
    return {
        Tier.FAST: FastFetcher(config),
        Tier.RENDER: RenderFetcher(config),
        Tier.STEALTH: StealthFetcher(config),
    }


def fetch(url: str, tier: Tier | None = None, timeout: int | None = None, config: Config | None = None) -> FetchResult:
    if config is None:
        config = load_config()

    effective_timeout = timeout or config.defaults.timeout
    fetchers = _build_fetchers(config)

    if config.cache.enabled:
        cache = FileCache(config.cache.dir, config.cache.ttl_seconds)
        cached = cache.get(url)
        if cached:
            _log("[cache] hit")
            return cached
    else:
        cache = None

    site_rule = match_site(url, config.sites)
    if site_rule and site_rule.tier and tier is None:
        tier = TIER_MAP.get(site_rule.tier)
        if tier:
            _log(f"[config] site rule matched: {site_rule.pattern} -> {site_rule.tier}")

    if tier:
        fetcher = fetchers[tier]
        if not fetcher.available():
            return FetchResult(url=url, tier=tier, status=FetchStatus.ERROR, error=f"{tier.value} fetcher not available")
        result = fetcher.fetch(url, effective_timeout)
    else:
        result = _auto_escalate(url, effective_timeout, config, fetchers)

    if cache and result.ok:
        cache.put(url, result)

    return result


def _auto_escalate(url: str, timeout: int, config: Config, fetchers: dict) -> FetchResult:
    tier_order = [TIER_MAP[t] for t in config.defaults.tier_order if t in TIER_MAP]
    start = time.monotonic()

    for tier in tier_order:
        elapsed = time.monotonic() - start
        remaining = max(5, timeout - int(elapsed))

        fetcher = fetchers[tier]
        if not fetcher.available():
            _log(f"[skip] {tier.value} - not available")
            continue

        _log(f"[try]  {tier.value}")
        result = fetcher.fetch(url, remaining)

        if result.ok:
            _log(f"[ok]   {tier.value} - {result.status_code}")
            return result

        if result.status == FetchStatus.BLOCKED:
            _log(f"[blocked] {tier.value} - escalating")
            continue

        if result.status == FetchStatus.JS_REQUIRED:
            _log(f"[js-required] {tier.value} - escalating")
            continue

        _log(f"[error] {tier.value} - {result.error}")

    return FetchResult(url=url, tier=tier_order[-1], status=FetchStatus.ERROR, error="all tiers exhausted")


def get_extraction_config(url: str, config: Config) -> ExtractionConfig | None:
    site_rule = match_site(url, config.sites)
    if site_rule and site_rule.extraction:
        return site_rule.extraction
    return config.defaults.extraction


def fetch_markdown(url: str, tier: Tier | None = None, timeout: int | None = None, config: Config | None = None) -> str:
    if config is None:
        config = load_config()
    result = fetch(url, tier, timeout, config)
    if not result.ok:
        return ""
    return html_to_markdown(result.html, url, get_extraction_config(url, config))


def fetch_text(url: str, tier: Tier | None = None, timeout: int | None = None, config: Config | None = None) -> str:
    if config is None:
        config = load_config()
    result = fetch(url, tier, timeout, config)
    if not result.ok:
        return ""
    return html_to_text(result.html, url, get_extraction_config(url, config))


_quiet = False


def set_quiet(quiet: bool):
    global _quiet
    _quiet = quiet


def _log(msg: str):
    if not _quiet:
        print(msg, file=sys.stderr)
