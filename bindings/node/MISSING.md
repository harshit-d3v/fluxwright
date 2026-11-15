# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- Mobile and touch emulation (`isMobile`, `hasTouch`), `emulateMedia` after the page opens
- `page.on('popup')`, which needs pages beyond the one a context holds
- `getByAltText`, `getByTitle`, `filter({ has })`, strict mode
- `connectOverCDP` in Node bindings (supported in Rust engine and HTTP server via `--cdp-endpoint`)
- `context.addCookies`, `setDefaultTimeout` and `setDefaultNavigationTimeout`

## Options that throw

napi would silently ignore an option the binding does not implement, so it throws a TypeError instead. The main ones:

- A per-call `timeout`, except on `waitForResponse`, `waitForRequest` and `waitForDownload`: the engine's own timeouts apply
- `waitForSelector` and `locator.waitFor` with a `state` other than `visible`, or `strict`
- Click and fill options (`button`, `clickCount`, `modifiers`, `position`, `force`, ...)
- Screenshots other than PNG (`type: 'jpeg'` or a `.jpg` path), and `quality`, `clip`, `omitBackground`, `mask`, ...
- `newPage` / `newContext` options the README does not list (`extraHTTPHeaders`, `javaScriptEnabled`, `ignoreHTTPSErrors`, `baseURL`, `httpCredentials`, `offline`, ...), and `launch` options other than its five (`args`; for `proxy`, give `newPage` one)
- `page.locator(selector, options)` (use `.filter({ hasText })`), `filter` options other than `hasText`, `getByRole` options other than `name` and `exact`
- `page.route(url, handler, { times })`, `route.fallback(overrides)`, `route.fulfill({ response })`, `context.cookies(urls)`, `page.close({ runBeforeUnload })`

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer, codegen and video recording
- Multiple pages per context: each context holds one page, so jobs never share cookies; share a login with `storageState`
- WebSocket and worker APIs
