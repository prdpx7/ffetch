from __future__ import annotations

from abc import ABC, abstractmethod
from typing import TYPE_CHECKING

from ffetch.types import FetchResult

if TYPE_CHECKING:
    from ffetch.config import Config


class BaseFetcher(ABC):
    def __init__(self, config: Config | None = None):
        self.config = config

    @abstractmethod
    def fetch(self, url: str, timeout: int = 30) -> FetchResult: ...

    @abstractmethod
    def available(self) -> bool: ...
