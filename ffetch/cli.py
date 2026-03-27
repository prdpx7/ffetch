from __future__ import annotations

import json
import platform
import shutil
import subprocess
import sys
from pathlib import Path

import click

from ffetch import __version__
from ffetch.config import load_config, _user_config_path
from ffetch.engine import fetch, get_extraction_config
from ffetch.convert import html_to_markdown, html_to_text
from ffetch.types import Tier

TIER_MAP = {"fast": Tier.FAST, "render": Tier.RENDER, "stealth": Tier.STEALTH, "auto": None}


class FfetchCLI(click.Group):
    def parse_args(self, ctx, args):
        if not args:
            return super().parse_args(ctx, args)
        first_non_flag = next((a for a in args if not a.startswith("-")), None)
        if first_non_flag and first_non_flag not in self.commands:
            args = ["get"] + args
        return super().parse_args(ctx, args)


@click.group(cls=FfetchCLI)
@click.version_option(__version__)
def main():
    """Smart web fetcher with auto-escalating tiers.

    \b
    ffetch <url>                  # auto-escalate, markdown output
    ffetch <url> -t fast          # force fast tier
    ffetch <url> --json           # structured JSON output
    ffetch <url> --cache          # enable response cache
    ffetch setup                  # one-time setup (config + lightpanda)
    ffetch config show            # print resolved config
    """


@main.command("get", hidden=True)
@click.argument("url")
@click.option("-t", "--tier", type=click.Choice(["auto", "fast", "render", "stealth"]), default="auto")
@click.option("-q", "--quiet", is_flag=True, help="Suppress debug logs on stderr")
@click.option("--json", "as_json", is_flag=True, help="Output as JSON")
@click.option("--html", "as_html", is_flag=True, help="Output raw HTML")
@click.option("--text", "as_text", is_flag=True, help="Output plain text instead of markdown")
@click.option("--timeout", default=None, type=int, help="Timeout in seconds")
@click.option("--cache", "use_cache", is_flag=True, help="Enable response caching")
@click.option("--config", "config_path", type=click.Path(), default=None, help="Path to config.yaml")
def get_url(url, tier, quiet, as_json, as_html, as_text, timeout, use_cache, config_path):
    """Fetch a URL."""
    from ffetch.engine import set_quiet
    if quiet:
        set_quiet(True)
    config = load_config(config_path)
    if use_cache:
        config.cache.enabled = True

    resolved_tier = TIER_MAP[tier]
    result = fetch(url, tier=resolved_tier, timeout=timeout, config=config)
    extraction = get_extraction_config(url, config)

    if as_json:
        out = {
            "url": result.url,
            "tier": result.tier.value,
            "status": result.status.value,
            "status_code": result.status_code,
            "error": result.error,
        }
        if as_html:
            out["content"] = result.html
        elif as_text:
            out["content"] = html_to_text(result.html, url, extraction) if result.ok else ""
        else:
            out["content"] = html_to_markdown(result.html, url, extraction) if result.ok else ""
        click.echo(json.dumps(out, indent=2))
        return

    if not result.ok:
        click.echo(f"error: {result.error or result.status.value}", err=True)
        sys.exit(1)

    if as_html:
        click.echo(result.html)
    elif as_text:
        click.echo(html_to_text(result.html, url, extraction))
    else:
        click.echo(html_to_markdown(result.html, url, extraction))


@main.group()
def config():
    """Manage ffetch configuration."""


@config.command("init")
def config_init():
    """Create config at ~/.config/ffetch/config.yaml"""
    target = _user_config_path()
    if target.exists():
        click.echo(f"config already exists: {target}")
        return
    template = Path(__file__).parent / "user_config_template.yaml"
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(template, target)
    click.echo(f"created: {target}")


@config.command("show")
@click.option("--config", "config_path", type=click.Path(), default=None)
def config_show(config_path):
    """Print resolved config as YAML."""
    import dataclasses
    import yaml

    cfg = load_config(config_path)

    def _to_dict(obj):
        if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
            return {k: _to_dict(v) for k, v in dataclasses.asdict(obj).items()}
        if isinstance(obj, list):
            return [_to_dict(i) for i in obj]
        return obj

    click.echo(yaml.dump(_to_dict(cfg), default_flow_style=False, sort_keys=False))


@config.command("path")
def config_path():
    """Print config file path."""
    click.echo(_user_config_path())


@main.command()
@click.option("--skip-lightpanda", is_flag=True, help="Skip lightpanda binary install")
@click.option("--skip-config", is_flag=True, help="Skip config file creation")
def setup(skip_lightpanda, skip_config):
    """One-time setup: config, lightpanda, and agent instructions.

    \b
    Optional MCP support:
      claude mcp add ffetch ffetch-mcp
    """
    if not skip_config:
        target = _user_config_path()
        if target.exists():
            click.echo(f"[ok] config exists: {target}")
        else:
            default = Path(__file__).parent / "user_config_template.yaml"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(default, target)
            click.echo(f"[ok] config created: {target}")

    if not skip_lightpanda:
        _install_lightpanda()

    claude_md = Path.home() / ".claude" / "CLAUDE.md"
    if claude_md.exists() and AGENT_MARKER in claude_md.read_text():
        click.echo(f"[ok] agent instructions already in {claude_md}")
    elif click.confirm("\nadd ffetch instructions to ~/.claude/CLAUDE.md?", default=True):
        _install_agent_prompt()

    _setup_claude_permission()

    click.echo("")
    click.echo("setup complete.")
    click.echo("")
    click.echo(f"  try it:       ffetch https://example.com")
    click.echo(f"  config:       ffetch config show")
    click.echo(f"  edit config:  $EDITOR {_user_config_path()}")


def _install_lightpanda():
    if shutil.which("lightpanda"):
        click.echo("[ok] lightpanda already installed")
        return

    machine = platform.machine().lower()
    system = platform.system().lower()

    arch_map = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}
    os_map = {"linux": "linux", "darwin": "macos"}

    arch = arch_map.get(machine)
    os_name = os_map.get(system)
    if not arch or not os_name:
        click.echo(f"[skip] lightpanda: unsupported platform {system}/{machine}")
        return

    binary_name = f"lightpanda-{arch}-{os_name}"
    url = f"https://github.com/lightpanda-io/browser/releases/download/nightly/{binary_name}"
    install_dir = Path.home() / ".local" / "bin"
    install_dir.mkdir(parents=True, exist_ok=True)
    dest = install_dir / "lightpanda"

    click.echo(f"[installing] lightpanda ({arch}-{os_name})...")
    try:
        subprocess.run(["curl", "-fSL", url, "-o", str(dest)], check=True, capture_output=True)
        dest.chmod(0o755)
        click.echo(f"[ok] lightpanda installed: {dest}")
    except (subprocess.CalledProcessError, FileNotFoundError) as e:
        click.echo(f"[error] lightpanda install failed: {e}")
        click.echo(f"  manual: curl -fSL {url} -o ~/.local/bin/lightpanda && chmod +x ~/.local/bin/lightpanda")


AGENT_PROMPT = """\

## Web Fetching

Always use `ffetch` CLI (via Bash) instead of WebFetch for fetching web content.
ffetch handles bot detection, JS rendering, and anti-block escalation automatically.
WebFetch fails on most real-world sites (Reddit, news sites, Cloudflare-protected pages).

- `ffetch <url>` returns clean markdown to stdout
- `ffetch <url> --json` for structured output (tier, status, content)
- `ffetch <url> --cache` to cache responses
- `ffetch <url> -t render` to force JS rendering for SPAs
- Config: `~/.config/ffetch/config.yaml` -- edit site rules to fix fetch issues
- Never use WebFetch. Always use `Bash(ffetch <url>)` instead.
"""

AGENT_MARKER = "## Web Fetching"


def _install_agent_prompt():
    path = Path.home() / ".claude" / "CLAUDE.md"

    if path.exists():
        content = path.read_text()
        if AGENT_MARKER in content:
            click.echo(f"[ok] agent prompt already in {path}")
            return
        path.write_text(content + AGENT_PROMPT)
        click.echo(f"[ok] agent prompt added to {path}")
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(AGENT_PROMPT.lstrip())
        click.echo(f"[ok] agent prompt created: {path}")


FFETCH_PERMISSION = "Bash(ffetch:*)"


def _setup_claude_permission():
    settings_path = Path.home() / ".claude" / "settings.json"
    if not settings_path.exists():
        return

    try:
        settings = json.loads(settings_path.read_text())
    except (json.JSONDecodeError, OSError):
        return

    allow = settings.get("permissions", {}).get("allow", [])
    if FFETCH_PERMISSION in allow:
        click.echo(f"[ok] claude permission already set")
        return

    if not click.confirm("\nallow `ffetch` in Claude Code settings.json?", default=True):
        return

    if "permissions" not in settings:
        settings["permissions"] = {}
    if "allow" not in settings["permissions"]:
        settings["permissions"]["allow"] = []

    settings["permissions"]["allow"].append(FFETCH_PERMISSION)
    settings_path.write_text(json.dumps(settings, indent=2) + "\n")
    click.echo(f"[ok] added {FFETCH_PERMISSION} to {settings_path}")
