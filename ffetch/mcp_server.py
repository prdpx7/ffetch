from mcp.server.fastmcp import FastMCP

from ffetch.config import load_config
from ffetch.convert import html_to_markdown
from ffetch.engine import fetch as _fetch, get_extraction_config
from ffetch.types import Tier

server = FastMCP("ffetch")

TIER_MAP = {"fast": Tier.FAST, "render": Tier.RENDER, "stealth": Tier.STEALTH, "auto": None}


@server.tool()
def ffetch(
    url: str,
    tier: str = "auto",
    output: str = "markdown",
    timeout: int = 30,
) -> str:
    """Fetch a URL with smart auto-escalation. Tries TLS impersonation first,
    escalates to JS rendering, then anti-detection if blocked.

    Behavior is controlled by ~/.config/ffetch/config.yaml -- edit site rules,
    tier preferences, blocked signals, and extraction settings there.

    Args:
        url: The URL to fetch
        tier: Fetch strategy - auto, fast, render, or stealth
        output: Output format - markdown, text, or html
        timeout: Timeout in seconds
    """
    config = load_config()
    resolved_tier = TIER_MAP.get(tier)
    result = _fetch(url, tier=resolved_tier, timeout=timeout, config=config)

    if not result.ok:
        return f"error: {result.error or result.status.value} (tier: {result.tier.value})"

    if output == "html":
        return result.html

    extraction = get_extraction_config(url, config)
    return html_to_markdown(result.html, url, extraction) or result.html


def main():
    server.run(transport="stdio")


if __name__ == "__main__":
    main()
