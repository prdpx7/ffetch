import hashlib
import json
import time
from pathlib import Path

from ffetch.types import FetchResult, FetchStatus, Tier


def _cache_key(url: str) -> str:
    return hashlib.sha256(url.encode()).hexdigest()[:16]


class FileCache:
    def __init__(self, cache_dir: str, ttl: int):
        self.dir = Path(cache_dir).expanduser()
        self.ttl = ttl

    def get(self, url: str) -> FetchResult | None:
        path = self.dir / f"{_cache_key(url)}.json"
        if not path.exists():
            return None
        try:
            data = json.loads(path.read_text())
            if time.time() - data["timestamp"] > self.ttl:
                path.unlink(missing_ok=True)
                return None
            return FetchResult(
                url=data["url"],
                tier=Tier(data["tier"]),
                status=FetchStatus(data["status"]),
                html=data["html"],
                status_code=data["status_code"],
                headers=data.get("headers", {}),
            )
        except (json.JSONDecodeError, KeyError, ValueError):
            path.unlink(missing_ok=True)
            return None

    def put(self, url: str, result: FetchResult) -> None:
        self.dir.mkdir(parents=True, exist_ok=True)
        path = self.dir / f"{_cache_key(url)}.json"
        data = {
            "url": result.url,
            "tier": result.tier.value,
            "status": result.status.value,
            "html": result.html,
            "status_code": result.status_code,
            "headers": result.headers,
            "timestamp": time.time(),
        }
        path.write_text(json.dumps(data))
