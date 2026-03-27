from dataclasses import dataclass, field
from enum import Enum


class Tier(Enum):
    FAST = "fast"
    RENDER = "render"
    STEALTH = "stealth"


class FetchStatus(Enum):
    OK = "ok"
    BLOCKED = "blocked"
    JS_REQUIRED = "js_required"
    ERROR = "error"


@dataclass
class FetchResult:
    url: str
    tier: Tier
    status: FetchStatus
    html: str = ""
    status_code: int = 0
    error: str = ""
    headers: dict = field(default_factory=dict)

    @property
    def ok(self) -> bool:
        return self.status == FetchStatus.OK and bool(self.html)
