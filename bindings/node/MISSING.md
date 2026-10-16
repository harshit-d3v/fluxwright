# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- Mobile and touch emulation (`isMobile`, `hasTouch`), `emulateMedia` after the page opens
- `page.on('popup')`, which needs pages beyond the one a context holds
- `getByAltText`, `getByTitle`, `filter({ has })`, strict mode
- `connectOverCDP`, to use browsers you already run
- `context.addCookies`, `setDefaultTimeout` and `setDefaultNavigationTimeout`

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer, codegen and video recording
- Multiple pages per context: each context holds one page, so jobs never share cookies; share a login with `storageState`
- WebSocket and worker APIs
