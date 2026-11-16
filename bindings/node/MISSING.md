# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- `page.on('popup')`, which needs pages beyond the one a context holds
- `filter({ has })`, strict mode

## Options that throw

napi would silently ignore an option the binding does not implement, so it throws a TypeError instead. The main ones:

- A per-call `timeout` on `title`, `content`, `evaluate`, `pdf` and `setViewportSize` (Playwright has none there either); `goto`, `click`, `fill`, `waitForSelector`, `screenshot` and the locator actions take one
- `waitForSelector` and `locator.waitFor` with `strict`
- Click and fill options (`button`, `clickCount`, `modifiers`, `position`, `force`, ...)
- Screenshots: `mask`, `maskColor`, `animations`, `caret`, `scale`, `style`, and WebP (see `docs/DECISIONS.md`)
- `page.pdf()` works only in headless mode (headed browsers reject with an error)
- `newPage` / `newContext` options the README does not list (`recordVideo`, ...), and `launch` options other than the supported ones (`args`; for `proxy`, give `newPage` one)
- `page.locator(selector, options)` (use `.filter({ hasText })`), `filter` options other than `hasText`, `getByRole` options other than `name` and `exact`
- `page.route(url, handler, { times })`, `route.fallback(overrides)`, `route.fulfill({ response })`, `context.cookies(urls)`, `page.close({ runBeforeUnload })`

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer, codegen and video recording
- Multiple pages per context: each context holds one page, so jobs never share cookies; share a login with `storageState`
- WebSocket and worker APIs
