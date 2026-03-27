import os
from dataclasses import dataclass, field
from fnmatch import fnmatch
from pathlib import Path
from urllib.parse import urlparse

import yaml


@dataclass
class ExtractionConfig:
    include_links: bool = True
    include_images: bool = False
    include_tables: bool = True
    favor_recall: bool = True
    fallback_extractor: str | None = "readability"


@dataclass
class DefaultsConfig:
    tier_order: list[str] = field(default_factory=lambda: ["fast", "render", "stealth"])
    timeout: int = 30
    impersonate: str = "chrome"
    html_size_threshold: int = 5000
    lightpanda_bin: str = "lightpanda"
    playwright_wait: str = "networkidle"
    playwright_sleep: float = 1.0
    stealth_sleep: float = 2.0
    blocked_signals: list[str] = field(default_factory=list)
    js_required_signals: list[str] = field(default_factory=list)
    extraction: ExtractionConfig = field(default_factory=ExtractionConfig)


@dataclass
class CacheConfig:
    enabled: bool = False
    dir: str = "~/.cache/ffetch"
    ttl_seconds: int = 3600


@dataclass
class SiteRule:
    pattern: str
    tier: str | None = None
    extraction: ExtractionConfig | None = None


@dataclass
class Config:
    defaults: DefaultsConfig = field(default_factory=DefaultsConfig)
    cache: CacheConfig = field(default_factory=CacheConfig)
    sites: list[SiteRule] = field(default_factory=list)


def _user_config_path() -> Path:
    xdg = os.environ.get("XDG_CONFIG_HOME", "")
    base = Path(xdg) if xdg else Path.home() / ".config"
    return base / "ffetch" / "config.yaml"


def _deep_merge(base: dict, override: dict) -> dict:
    merged = base.copy()
    for k, v in override.items():
        if k in merged and isinstance(merged[k], dict) and isinstance(v, dict):
            merged[k] = _deep_merge(merged[k], v)
        else:
            merged[k] = v
    return merged


def _extraction_from_dict(d: dict) -> ExtractionConfig:
    return ExtractionConfig(**{k: v for k, v in d.items() if k in ExtractionConfig.__dataclass_fields__})


def _config_from_dict(d: dict) -> Config:
    defaults_raw = d.get("defaults", {})
    extraction_raw = defaults_raw.pop("extraction", {})
    defaults = DefaultsConfig(**{k: v for k, v in defaults_raw.items() if k in DefaultsConfig.__dataclass_fields__})
    defaults.extraction = _extraction_from_dict(extraction_raw) if extraction_raw else ExtractionConfig()

    cache_raw = d.get("cache", {})
    cache = CacheConfig(**{k: v for k, v in cache_raw.items() if k in CacheConfig.__dataclass_fields__})

    sites = []
    for s in d.get("sites", []):
        ext = s.pop("extraction", None)
        rule = SiteRule(
            pattern=s["pattern"],
            tier=s.get("tier"),
            extraction=_extraction_from_dict(ext) if ext else None,
        )
        sites.append(rule)

    return Config(defaults=defaults, cache=cache, sites=sites)


def _load_yaml(path: Path) -> dict:
    if not path.exists():
        return {}
    with open(path) as f:
        return yaml.safe_load(f) or {}


def _bundled_defaults() -> dict:
    default_yaml = Path(__file__).parent / "default_config.yaml"
    return _load_yaml(default_yaml)


def load_config(config_path: str | None = None) -> Config:
    base = _bundled_defaults()
    user_path = Path(config_path) if config_path else _user_config_path()
    user = _load_yaml(user_path)
    merged = _deep_merge(base, user)
    return _config_from_dict(merged)


def match_site(url: str, sites: list[SiteRule]) -> SiteRule | None:
    hostname = urlparse(url).hostname or ""
    for rule in sites:
        pattern = rule.pattern
        if fnmatch(hostname, pattern):
            return rule
        if pattern.startswith("*.") and fnmatch(hostname, pattern[2:]):
            return rule
    return None
