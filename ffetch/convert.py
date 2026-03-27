from __future__ import annotations

from typing import TYPE_CHECKING

import trafilatura

if TYPE_CHECKING:
    from ffetch.config import ExtractionConfig

MIN_USEFUL_LENGTH = 200
MIN_HTML_FOR_FALLBACK = 10_000


def _trafilatura_extract(html: str, url: str, fmt: str, extraction: ExtractionConfig | None = None) -> str:
    include_links = extraction.include_links if extraction else True
    include_images = extraction.include_images if extraction else False
    include_tables = extraction.include_tables if extraction else True
    favor_recall = extraction.favor_recall if extraction else True

    result = trafilatura.extract(
        html,
        url=url,
        output_format=fmt,
        include_links=include_links if fmt == "markdown" else False,
        include_images=include_images,
        include_tables=include_tables,
        favor_recall=favor_recall,
    )
    return result or ""


def _readability_fallback(html: str, url: str, fmt: str, extraction: ExtractionConfig | None = None) -> str:
    try:
        from readability import Document
    except ImportError:
        return ""

    doc = Document(html, url=url)
    cleaned = doc.summary(html_partial=False)
    return _trafilatura_extract(cleaned, url, fmt, extraction)


def _extract(html: str, url: str, fmt: str, extraction: ExtractionConfig | None = None) -> str:
    result = _trafilatura_extract(html, url, fmt, extraction)

    use_fallback = (
        extraction and extraction.fallback_extractor == "readability"
    ) or (extraction is None)

    if len(result) < MIN_USEFUL_LENGTH and len(html) > MIN_HTML_FOR_FALLBACK and use_fallback:
        fallback = _readability_fallback(html, url, fmt, extraction)
        if len(fallback) > len(result):
            return fallback

    return result


def html_to_markdown(html: str, url: str = "", extraction: ExtractionConfig | None = None) -> str:
    return _extract(html, url, "markdown", extraction)


def html_to_text(html: str, url: str = "", extraction: ExtractionConfig | None = None) -> str:
    return _extract(html, url, "txt", extraction)
