"""Run hundreds of headless Chrome jobs on one machine, with a Playwright-style API.

``fluxwright.async_api`` is for asyncio code and ``fluxwright.sync_api`` for blocking code; each
mirrors the Playwright module of the same name.
"""

from importlib.metadata import PackageNotFoundError as _PackageNotFoundError
from importlib.metadata import version as _version

from ._errors import Error, TimeoutError
from .client import AsyncFluxwrightClient, FluxwrightApiError, FluxwrightClient

__all__ = [
    "AsyncFluxwrightClient",
    "Error",
    "FluxwrightApiError",
    "FluxwrightClient",
    "TimeoutError",
]
try:
    __version__ = _version("fluxwright")
except _PackageNotFoundError:
    __version__ = "0.0.0.dev0"
