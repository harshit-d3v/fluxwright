//! Machine-readable OpenAPI 3.1.0 specification served at `GET /openapi.json`.

use axum::Json;
use serde_json::{json, Value};

pub(crate) async fn openapi_json() -> Json<Value> {
    Json(build_openapi_spec())
}

pub(crate) fn build_openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Fluxwright HTTP API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "High-concurrency 3-tier Chromium fleet, pure-Rust HTTP scraping, recursive site crawling, managed browser sessions, and CDP gateway.",
            "license": { "name": "Apache-2.0" }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            }
        },
        "security": [{ "bearerAuth": [] }],
        "paths": {
            "/healthz": {
                "get": {
                    "summary": "Liveness probe",
                    "security": [],
                    "responses": { "200": { "description": "Server alive" } }
                }
            },
            "/openapi.json": {
                "get": {
                    "summary": "OpenAPI 3.1 schema document",
                    "security": [],
                    "responses": { "200": { "description": "OpenAPI JSON schema" } }
                }
            },
            "/v1/scrape": {
                "post": {
                    "summary": "Synchronously scrape markdown, HTML, text, snapshot, screenshot, PDF, and CSS-extracted JSON",
                    "responses": {
                        "200": { "description": "Scraped page payload" },
                        "400": { "description": "Invalid request or SSRF block" }
                    }
                }
            },
            "/v1/map": {
                "post": {
                    "summary": "Discover links across a site with optional XML sitemap parsing",
                    "responses": { "200": { "description": "Discovered links" } }
                }
            },
            "/v1/crawl": {
                "post": {
                    "summary": "Start an asynchronous breadth-first recursive crawl job",
                    "responses": { "202": { "description": "Crawl accepted" } }
                }
            },
            "/v1/crawl/{id}": {
                "get": {
                    "summary": "Get crawl progress and crawled page results",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Crawl snapshot" }, "404": { "description": "Crawl not found" } }
                },
                "delete": {
                    "summary": "Cancel a running crawl job",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Crawl cancelled" }, "404": { "description": "Crawl not found" } }
                }
            },
            "/v1/jobs": {
                "post": {
                    "summary": "Submit an imperative multi-step Chromium job",
                    "responses": { "202": { "description": "Job queued" } }
                }
            },
            "/v1/jobs/{id}": {
                "get": { "summary": "Inspect job status and step results", "responses": { "200": { "description": "Job snapshot" } } },
                "delete": { "summary": "Cancel a queued or running job", "responses": { "200": { "description": "Job cancelled" } } }
            },
            "/v1/sessions": {
                "post": { "summary": "Allocate a persistent managed browser session with CDP and Live View", "responses": { "201": { "description": "Session created" } } },
                "get": { "summary": "List active managed browser sessions", "responses": { "200": { "description": "Active sessions" } } }
            },
            "/v1/sessions/{id}": {
                "get": { "summary": "Get session status", "responses": { "200": { "description": "Session metadata" } } },
                "delete": { "summary": "Release a managed session", "responses": { "200": { "description": "Session deleted" } } }
            },
            "/v1/sessions/{id}/extend": {
                "post": { "summary": "Extend session TTL", "responses": { "200": { "description": "TTL extended" } } }
            },
            "/v1/sessions/{id}/replay": {
                "get": { "summary": "Fetch retained session screencast frames", "responses": { "200": { "description": "Replay frames" } } }
            },
            "/v1/sessions/{id}/network": {
                "get": { "summary": "Fetch captured session network entries", "responses": { "200": { "description": "Network entries" } } }
            },
            "/v1/sessions/{id}/cdp": {
                "get": { "summary": "WebSocket CDP endpoint pinned to a managed session", "responses": { "101": { "description": "WebSocket upgraded" } } }
            },
            "/v1/stats": {
                "get": { "summary": "Server queue and browser pool telemetry", "responses": { "200": { "description": "Server stats" } } }
            },
            "/v1/profiles": {
                "get": { "summary": "List persisted browser profiles", "responses": { "200": { "description": "Profile list" } } }
            },
            "/v1/proxies": {
                "get": { "summary": "List proxy pool health", "responses": { "200": { "description": "Proxy metrics" } } }
            },
            "/v1/live": {
                "get": { "summary": "List active Live View sessions", "responses": { "200": { "description": "Live sessions" } } }
            },
            "/metrics": {
                "get": { "summary": "Prometheus text exposition", "responses": { "200": { "description": "Metrics text" } } }
            },
            "/cdp": {
                "get": { "summary": "Pooled browser WebSocket CDP gateway", "responses": { "101": { "description": "WebSocket upgraded" } } }
            },
            "/json/version": {
                "get": { "summary": "CDP browser version discovery metadata", "responses": { "200": { "description": "CDP discovery JSON" } } }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_spec_lists_core_endpoints() {
        let spec = build_openapi_spec();
        assert_eq!(spec["openapi"], "3.1.0");
        assert!(spec["paths"]["/v1/crawl"]["post"].is_object());
        assert!(spec["paths"]["/v1/crawl/{id}"]["get"].is_object());
        assert!(spec["paths"]["/v1/scrape"]["post"].is_object());
        assert!(spec["paths"]["/openapi.json"]["get"].is_object());
    }
}
