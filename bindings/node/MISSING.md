# Playwright methods not in Fluxwright (TypeScript)

The TS binding covers the same small page API as the Rust crate. Missing on purpose:

- Firefox / WebKit
- Tracing, HAR, video
- `page.route` beyond resource-type / URL blocking on the job
- Network idle waits as first-class APIs
- Storage state import/export
- `page.emulateMedia`, geolocation, permissions helpers
- Playwright Test runner, fixtures, expect
- `locator.filter`, `getByRole`, `getByText` (basic CSS locator only)
- Multiple pages per context
- Download / upload helpers
- WebSocket / worker APIs
- `connectOverCDP` (planned as a later `BrowserSource`)
