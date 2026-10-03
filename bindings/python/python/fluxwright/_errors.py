class Error(Exception):
    """A Fluxwright failure, raised as Playwright raises its ``Error``.

    Also what ``pageerror`` listeners and ``page.page_errors()`` get for an exception the page
    did not catch: ``name`` is then the JavaScript error's (``TypeError``, ...) and ``stack`` its
    stack.
    """

    def __init__(self, message: str, name: str = "Error", stack: str = "") -> None:
        super().__init__(message)
        self.message = message
        self.name = name
        self.stack = stack


class TimeoutError(Error):  # noqa: A001 - Playwright's name for it
    """Something ran out of time: waiting for a page slot, an element, a download, ..."""

    def __init__(self, message: str, name: str = "TimeoutError", stack: str = "") -> None:
        super().__init__(message, name, stack)
