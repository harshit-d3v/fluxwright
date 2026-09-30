# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- Saved login state: `storageState` import and export
- `page.route` to fulfill or modify requests (today: blocking by resource type or URL)
- Emulation: user agent, locale, timezone, geolocation, permissions, `emulateMedia`
- `locator.screenshot()`
- `getByLabel`, `getByPlaceholder`, `getByTestId`, `nth`, `filter`
- Downloads
- `connectOverCDP`, to use browsers you already run
- Python bindings

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer, codegen and video recording
- Multiple pages per context
- WebSocket and worker APIs
