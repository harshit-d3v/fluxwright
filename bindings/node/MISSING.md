# Playwright features not in Fluxwright

The Node binding covers the same page API as the Rust crate. Some Playwright features are planned, and some are out of scope.

## Planned (see [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md))

- See [ROADMAP.md](https://github.com/harshit-d3v/fluxwright/blob/main/ROADMAP.md) for upcoming CDP escape hatches, networking, and fleet features

## Options that throw

napi would silently ignore an option the binding does not implement, so it throws a TypeError instead. The main ones:

- A per-call `timeout` on `title`, `content`, `evaluate`, `pdf` and `setViewportSize` (Playwright has none there either); `goto`, `click`, `dblclick`, `hover`, `focus`, `blur`, `scrollIntoViewIfNeeded`, `dragTo`, `dragAndDrop`, `fill`, `press`, `pressSequentially`, `clear`, `check`, `uncheck`, `setChecked`, `selectOption`, `setInputFiles`, `tap`, `waitForSelector`, `waitForFileChooser`, `screenshot` and the locator actions take one
- Pointer/input options beyond the implemented subset (`noWaitAfter`, `sourcePosition` / `targetPosition` on `dragTo` / `dragAndDrop`)
- Screenshots: `mask`, `maskColor`, `animations`, `caret`, `scale`, `style`, and WebP (see `docs/DECISIONS.md`)
- `page.pdf()` works only in headless mode (headed browsers reject with an error)
- `newPage` / `newContext` options the README does not list, and `launch` options other than the supported ones (`args`; for `proxy`, give `newPage` one)
- `page.route(url, handler, { times })`, `route.fallback(overrides)`, `page.close({ runBeforeUnload })`

## Out of scope

- Firefox and WebKit
- Playwright Test: the runner, fixtures and `expect`
- Tracing, the trace viewer and codegen
