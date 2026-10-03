"""Types the async and sync APIs share."""

from collections.abc import Callable
from re import Pattern
from typing import Any, TypedDict

#: A glob (``**`` any characters, ``*`` any but ``/``, ``{a,b}`` either), a compiled regular
#: expression (searched), or a function of the URL.
URLMatch = str | Pattern[str] | Callable[[str], bool]
#: Cookies and localStorage in Playwright's ``storage_state`` format, so files work in both.
StorageState = dict[str, Any]


class ViewportSize(TypedDict):
    width: int
    height: int


class FloatRect(TypedDict):
    x: float
    y: float
    width: float
    height: float


class _ProxyServer(TypedDict):
    server: str


class ProxySettings(_ProxyServer, total=False):
    """``server`` such as ``http://host:port`` or ``socks5://host:port``; ``bypass`` is a
    comma-separated list of hosts that skip the proxy."""

    bypass: str
    username: str
    password: str


class _Coordinates(TypedDict):
    latitude: float
    longitude: float


class Geolocation(_Coordinates, total=False):
    accuracy: float
