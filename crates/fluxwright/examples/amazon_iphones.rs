//! Opens a visible Chrome window, searches Amazon for Apple iPhone 13–18,
//! and prints one listing price per model.
//!
//! Default store is amazon.in (India). Override with AMAZON_HOST=www.amazon.com

use std::time::Duration;

use fluxwright::{BrowserEngine, JobOptions};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fluxwright=info".parse()?),
        )
        .init();

    let host = std::env::var("AMAZON_HOST").unwrap_or_else(|_| "www.amazon.in".into());
    let engine = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .headless(false)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await?;

    println!("Chrome will open. Searching {host} for iPhone 13–18…\n");
    println!("{:<10}  {:<12}  {}", "model", "price", "title");
    println!("{}", "-".repeat(88));

    for model in 13u32..=18 {
        let query = format!("Apple iPhone {model}");
        let url = format!(
            "https://{host}/s?k={}",
            urlencoding_lite(&query)
        );
        let found = engine
            .run(
                JobOptions::default().timeout(Duration::from_secs(90)),
                {
                    let url = url.clone();
                    move |page| {
                        let url = url.clone();
                        async move {
                            page.goto(&url).await?;
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            let needle = format!("iPhone {model}");
                            let expr = format!(
                                r#"(function() {{
                                    const needle = {needle};
                                    const skip = /case|cover|charger|cable|protector|tempered|holder|pouch|skin|strap|bumper/i;
                                    const cards = document.querySelectorAll('[data-component-type="s-search-result"]');
                                    const hits = [];
                                    for (const c of cards) {{
                                        const title = (c.querySelector('h2 span') || c.querySelector('h2 a') || {{}}).innerText || '';
                                        const priceEl = c.querySelector('.a-price .a-offscreen') || c.querySelector('.a-price-whole');
                                        const price = priceEl ? priceEl.textContent.trim() : '';
                                        if (!title || skip.test(title)) continue;
                                        if (title.toLowerCase().indexOf(needle.toLowerCase()) === -1) continue;
                                        hits.push({{ title: title.trim(), price: price || '(no price)' }});
                                    }}
                                    const captcha = !!(document.getElementById('captchacharacters') || document.title.match(/robot|captcha/i));
                                    return {{ captcha, title: document.title, hits }};
                                }})()"#,
                                needle = json!(needle)
                            );
                            page.evaluate(&expr).await
                        }
                    }
                },
            )
            .await;

        match found {
            Ok(v) => {
                if v["captcha"].as_bool() == Some(true) {
                    println!(
                        "{:<10}  {:<12}  Amazon showed a captcha — solve it in the Chrome window and re-run",
                        format!("iPhone {model}"),
                        "-"
                    );
                    continue;
                }
                let hits = v["hits"].as_array().cloned().unwrap_or_default();
                if hits.is_empty() {
                    println!(
                        "{:<10}  {:<12}  not listed (page: {})",
                        format!("iPhone {model}"),
                        "-",
                        v["title"].as_str().unwrap_or("?")
                    );
                } else {
                    let first = &hits[0];
                    println!(
                        "{:<10}  {:<12}  {}",
                        format!("iPhone {model}"),
                        first["price"].as_str().unwrap_or("-"),
                        first["title"].as_str().unwrap_or("")
                    );
                }
            }
            Err(e) => {
                println!(
                    "{:<10}  {:<12}  error: {e}",
                    format!("iPhone {model}"),
                    "-"
                );
            }
        }
    }

    println!("\nDone. Close the Chrome window or press Ctrl+C.");
    engine.shutdown(Duration::from_secs(10)).await.ok();
    Ok(())
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("+"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
