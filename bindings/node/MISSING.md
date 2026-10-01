# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- `page.route` to fulfill or modify requests (today: blocking by resource type or URL)
- Mobile and touch emulation (`isMobile`, `hasTouch`), `emulateMedia` after the page opens
- `page.on('console' | 'pageerror')` callbacks (today: `consoleMessages()` and `pageErrors()`)
- `getByAltText`, `getByTitle`, `filter({ has })`, strict mode
- Downloads
- `connectOverCDP`, to use browsers you already run
- Python bindings

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer, codegen and video recording
- Multiple pages per context
- WebSocket and worker APIs
