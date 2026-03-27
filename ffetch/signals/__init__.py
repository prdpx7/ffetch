from functools import cache
from pathlib import Path

SIGNALS_DIR = Path(__file__).parent


@cache
def load_signals(name: str) -> list[str]:
    path = SIGNALS_DIR / f"{name}.txt"
    if not path.exists():
        return []
    return [line.strip() for line in path.read_text().splitlines() if line.strip()]


def blocked_signals() -> list[str]:
    return load_signals("blocked")


def js_required_signals() -> list[str]:
    return load_signals("js_required")
