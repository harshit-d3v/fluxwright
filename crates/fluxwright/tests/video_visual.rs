use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{response::Html, routing::get, Router};
use fluxwright::{
    decode_png, encode_mjpeg_avi, expect_locator, expect_page, Animations, BrowserEngine,
    JobOptions, RecordVideoOptions, ToHaveScreenshotOptions,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fluxwright-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn engine() -> BrowserEngine {
    BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .queue_capacity(64)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .expect("engine")
}

fn assert_valid_video_header(bytes: &[u8]) {
    assert!(bytes.len() > 16, "video file too small: {} bytes", bytes.len());
    let is_webm = bytes.starts_with(b"\x1a\x45\xdf\xa3");
    let is_avi = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"AVI ";
    assert!(
        is_webm || is_avi,
        "expected WebM (1A 45 DF A3) or AVI (RIFF....AVI ) header, got {:02x?}",
        &bytes[..bytes.len().min(16)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn video_recording_save_as_and_delete() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <style>
                  body { margin: 0; background: #fff; }
                  #box { width: 120px; height: 120px; background: #e11d48; transition: background 0.05s; }
                </style>
                <div id="box">Video Test</div>
                <script>
                  setTimeout(() => { document.getElementById('box').style.background = '#2563eb'; }, 40);
                  setTimeout(() => { document.getElementById('box').textContent = 'Frame 2'; }, 90);
                </script>"#,
            )
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/");

    let tmp = TestDir::new("video");
    let video_dir = tmp.path().join("videos");
    let eng = engine().await;

    // Without record_video, page.video() returns None.
    {
        let plain_page = eng.acquire().await.unwrap();
        assert!(plain_page.video().is_none());
        plain_page.close().await.unwrap();
    }

    // Context-level record_video + explicit page.close() + save_as() + delete().
    {
        let opts = JobOptions::default()
            .viewport(640, 480)
            .record_video(RecordVideoOptions::new(&video_dir).size(640, 480));
        let ctx = eng.acquire_context_with(&opts).await.unwrap();
        let page = ctx.new_page().await.unwrap();
        let video = page.video().expect("video handle must be present");

        page.goto(&url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(160)).await;
        page.close().await.unwrap();

        let recorded_path = video.path().await.unwrap();
        assert!(recorded_path.starts_with(&video_dir), "{recorded_path:?}");
        assert!(recorded_path.exists(), "recorded file must exist");
        let bytes = std::fs::read(&recorded_path).unwrap();
        assert_valid_video_header(&bytes);

        let saved_dest = tmp.path().join("exports").join("copy.video");
        video.save_as(&saved_dest).await.unwrap();
        assert_eq!(std::fs::read(&saved_dest).unwrap(), bytes);

        video.delete().await.unwrap();
        assert!(!recorded_path.exists(), "video.delete() must remove recorded file");
        // Deleting a second time is idempotent.
        video.delete().await.unwrap();

        ctx.close().await.unwrap();
    }

    // Job-level record_video + drop(page) finalizes cleanly too.
    {
        let opts = JobOptions::default()
            .viewport(400, 300)
            .record_video(RecordVideoOptions::new(&video_dir));
        let page = eng.acquire_job(&opts).await.unwrap();
        let video = page.video().expect("job video must be present");
        page.goto(&url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(page);

        let recorded_path = video.path().await.unwrap();
        assert!(recorded_path.exists());
        assert_valid_video_header(&std::fs::read(&recorded_path).unwrap());
    }

    // Verify pure-Rust MJPEG-in-AVI fallback container structure directly.
    let minimal_jpeg = [0xffu8, 0xd8, 0xff, 0xd9];
    let avi = encode_mjpeg_avi(&[&minimal_jpeg], 10, 320, 240);
    assert_eq!(&avi[0..4], b"RIFF");
    assert_eq!(&avi[8..12], b"AVI ");
    assert!(Path::new(&video_dir).exists());

    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn visual_regression_to_have_screenshot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <style>
                  body { margin: 0; background: #ffffff; }
                  #box {
                    position: relative;
                    width: 80px;
                    height: 80px;
                    background: rgb(255, 0, 0);
                  }
                  #dot {
                    position: absolute;
                    top: 0;
                    left: 0;
                    width: 2px;
                    height: 2px;
                    background: rgb(255, 0, 0);
                  }
                  @keyframes pulse {
                    0% { background: rgb(255, 0, 0); }
                    50% { background: rgb(0, 255, 0); }
                    100% { background: rgb(0, 0, 255); }
                  }
                  #anim {
                    width: 40px;
                    height: 40px;
                    background: rgb(255, 0, 0);
                    animation: pulse 0.2s linear infinite;
                  }
                </style>
                <div id="box"><div id="dot"></div></div>
                <div id="anim"></div>"#,
            )
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/");

    let tmp = TestDir::new("visual");
    let box_shot = tmp.path().join("snapshots").join("box.png");
    let box_actual = tmp.path().join("snapshots").join("box-actual.png");
    let box_diff = tmp.path().join("snapshots").join("box-diff.png");
    let env_shot = tmp.path().join("snapshots").join("env-box.png");
    let anim_shot = tmp.path().join("snapshots").join("anim.png");
    let page_shot = tmp.path().join("snapshots").join("page.png");

    let eng = engine().await;
    let page = eng.acquire_job(&JobOptions::default().viewport(320, 240)).await.unwrap();
    page.goto(&url).await.unwrap();

    // 1. Missing baseline fails cleanly without creating baseline.
    let missing_err = expect_locator(&page.locator("#box"))
        .timeout(Duration::from_millis(250))
        .to_have_screenshot(&box_shot)
        .await
        .unwrap_err();
    assert!(!box_shot.exists(), "missing baseline must not be created without update");
    assert!(box_actual.exists(), "missing baseline writes -actual.png");
    assert!(missing_err.to_string().contains("baseline snapshot does not exist"));

    // 2. Explicit update(true) creates golden baseline, and exact match passes.
    expect_locator(&page.locator("#box"))
        .to_have_screenshot_with(&box_shot, &ToHaveScreenshotOptions::default().update(true))
        .await
        .unwrap();
    assert!(box_shot.exists());
    expect_locator(&page.locator("#box"))
        .to_have_screenshot(&box_shot)
        .await
        .unwrap();

    // 3. FLUXWRIGHT_UPDATE_SNAPSHOTS=1 creates golden baseline too.
    let prev_env = std::env::var_os("FLUXWRIGHT_UPDATE_SNAPSHOTS");
    std::env::set_var("FLUXWRIGHT_UPDATE_SNAPSHOTS", "1");
    let env_res = expect_locator(&page.locator("#box"))
        .to_have_screenshot(&env_shot)
        .await;
    match prev_env {
        Some(v) => std::env::set_var("FLUXWRIGHT_UPDATE_SNAPSHOTS", v),
        None => std::env::remove_var("FLUXWRIGHT_UPDATE_SNAPSHOTS"),
    }
    env_res.unwrap();
    assert!(env_shot.exists());

    // 4. Auto-retrying polling passes once delayed DOM change restores baseline appearance.
    page.evaluate(
        "document.getElementById('box').style.background = 'rgb(0, 255, 0)';
         setTimeout(() => { document.getElementById('box').style.background = 'rgb(255, 0, 0)'; }, 120);",
    )
    .await
    .unwrap();
    expect_locator(&page.locator("#box"))
        .timeout(Duration::from_secs(3))
        .to_have_screenshot(&box_shot)
        .await
        .unwrap();

    // 5. Threshold tolerance pass vs strict threshold=0.0 fail.
    page.evaluate("document.getElementById('box').style.background = 'rgb(225, 0, 0)';")
        .await
        .unwrap();
    expect_locator(&page.locator("#box"))
        .to_have_screenshot(&box_shot)
        .await
        .unwrap();
    let strict_err = expect_locator(&page.locator("#box"))
        .timeout(Duration::from_millis(250))
        .to_have_screenshot_with(&box_shot, &ToHaveScreenshotOptions::default().threshold(0.0))
        .await
        .unwrap_err();
    assert!(strict_err.is_timeout());

    // 6. Restore background; test max_diff_pixels and max_diff_pixel_ratio with small 2x2 blue dot.
    page.evaluate(
        "document.getElementById('box').style.background = 'rgb(255, 0, 0)';
         document.getElementById('dot').style.background = 'rgb(0, 0, 255)';",
    )
    .await
    .unwrap();
    expect_locator(&page.locator("#box"))
        .to_have_screenshot_with(&box_shot, &ToHaveScreenshotOptions::default().max_diff_pixels(16))
        .await
        .unwrap();
    expect_locator(&page.locator("#box"))
        .to_have_screenshot_with(
            &box_shot,
            &ToHaveScreenshotOptions::default().max_diff_pixel_ratio(0.01),
        )
        .await
        .unwrap();

    // 7. Mismatch with 0 tolerance writes -actual.png & -diff.png (with magenta diff pixels) and clear error.
    let _ = std::fs::remove_file(&box_actual);
    let _ = std::fs::remove_file(&box_diff);
    let mismatch_err = expect_locator(&page.locator("#box"))
        .timeout(Duration::from_millis(250))
        .to_have_screenshot(&box_shot)
        .await
        .unwrap_err();
    let err_msg = mismatch_err.to_string();
    assert!(err_msg.contains("pixels differ"), "{err_msg}");
    assert!(err_msg.contains("ratio "), "{err_msg}");
    assert!(err_msg.contains("box-actual.png"), "{err_msg}");
    assert!(err_msg.contains("box-diff.png"), "{err_msg}");
    assert!(box_actual.exists(), "-actual.png must be written on mismatch");
    assert!(box_diff.exists(), "-diff.png must be written on mismatch");
    let diff_img = decode_png(&std::fs::read(&box_diff).unwrap()).unwrap();
    assert!(
        diff_img.data.chunks_exact(4).any(|px| px == [255, 0, 255, 255]),
        "diff PNG must contain magenta (#ff00ff) diff pixels"
    );

    // 8. animations: Disabled pauses CSS animations deterministically.
    expect_locator(&page.locator("#anim"))
        .to_have_screenshot_with(
            &anim_shot,
            &ToHaveScreenshotOptions::default()
                .animations(Animations::Disabled)
                .update(true),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    expect_locator(&page.locator("#anim"))
        .to_have_screenshot(&anim_shot)
        .await
        .unwrap();

    // 9. Page-level to_have_screenshot.
    expect_page(&page)
        .to_have_screenshot_with(&page_shot, &ToHaveScreenshotOptions::default().update(true))
        .await
        .unwrap();
    expect_page(&page)
        .to_have_screenshot(&page_shot)
        .await
        .unwrap();
    page.evaluate("document.body.style.background = '#000000';")
        .await
        .unwrap();
    let page_err = expect_page(&page)
        .timeout(Duration::from_millis(250))
        .to_have_screenshot(&page_shot)
        .await
        .unwrap_err();
    assert!(page_err.to_string().contains("page-diff.png"));
    assert!(tmp.path().join("snapshots").join("page-actual.png").exists());
    assert!(tmp.path().join("snapshots").join("page-diff.png").exists());

    page.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}
