use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use tauri::Manager;

pub async fn ensure_api_webview(
    app: &tauri::AppHandle,
    result_store: &Arc<std::sync::Mutex<Option<String>>>,
    portal_name: &str,
) -> anyhow::Result<tauri::WebviewWindow> {
    if let Some(existing) = app.get_webview_window("udemy-api") {
        tracing::info!("[udemy-webview] Reusing existing webview window");
        return Ok(existing);
    }

    let store = result_store.clone();
    let base_url = format!("https://{}.udemy.com/", portal_name);

    tracing::info!("[udemy-webview] Creating webview window for: {}", base_url);

    let window = tauri::WebviewWindowBuilder::new(
        app,
        "udemy-api",
        tauri::WebviewUrl::External(base_url.parse().unwrap()),
    )
    .visible(true)
    .title("OmniGet - Udemy Debug (pass Cloudflare check then wait)")
    .inner_size(1000.0, 750.0)
    .initialization_script(
        r#"
        console.log('[omniget] initialization script loaded');
        console.log('[omniget] current URL:', window.location.href);

        window.addEventListener('load', function() {
            console.log('[omniget] page loaded:', window.location.href);
            if (document.title.includes('Just a moment') || 
                document.title.includes('Attention Required') ||
                document.title.includes('Checking')) {
                console.warn('[omniget] CLOUDFLARE CHALLENGE DETECTED!');
            }
        });

        // Send data in chunks to avoid URL length limits
        window.__omniget_send_result = function(text) {
            var CHUNK_SIZE = 8000;
            if (text.length <= CHUNK_SIZE) {
                console.log('[omniget] sending result in single chunk, length:', text.length);
                window.location.href = 'https://omniget-api-result.local/?single=' + encodeURIComponent(text);
                return;
            }
            var total = Math.ceil(text.length / CHUNK_SIZE);
            console.log('[omniget] sending result in', total, 'chunks, total length:', text.length);
            var i = 0;
            function sendNext() {
                if (i >= total) {
                    console.log('[omniget] all chunks sent, sending done signal');
                    window.location.href = 'https://omniget-api-result.local/?done=1';
                    return;
                }
                var chunk = text.substring(i * CHUNK_SIZE, (i + 1) * CHUNK_SIZE);
                console.log('[omniget] sending chunk', i + 1, '/', total, 'size:', chunk.length);
                window.location.href = 'https://omniget-api-result.local/?chunk=' + i + '&d=' + encodeURIComponent(chunk);
                i++;
                setTimeout(sendNext, 50);
            }
            sendNext();
        };

        window.__omniget_fetch = function(url) {
            console.log('[omniget] === FETCH START ===');
            console.log('[omniget] fetching:', url);

            fetch(url, {
                credentials: 'include',
                headers: {
                    'Accept': 'application/json, text/plain, */*',
                    'X-Requested-With': 'XMLHttpRequest'
                }
            })
            .then(function(r) {
                console.log('[omniget] response status:', r.status, r.statusText);
                return r.text();
            })
            .then(function(text) {
                console.log('[omniget] response length:', text.length);
                if (text.length === 0) {
                    console.error('[omniget] EMPTY RESPONSE');
                }
                window.__omniget_send_result(text);
            })
            .catch(function(err) {
                console.error('[omniget] fetch error:', err.message);
                window.location.href = 'https://omniget-api-result.local/?error=' + encodeURIComponent(err.message);
            });
        };
        "#,
    )
    .on_navigation(move |url| {
        if url.host_str() == Some("omniget-api-result.local") {
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "single" => {
                        tracing::info!("[udemy-webview] Got single-chunk result, length: {}", value.len());
                        *store.lock().unwrap() = Some(value.to_string());
                        return false;
                    }
                    "chunk" => {
                        // Get chunk data from 'd' parameter
                        for (k2, v2) in url.query_pairs() {
                            if k2 == "d" {
                                let mut guard = store.lock().unwrap();
                                let current = guard.get_or_insert_with(String::new);
                                if current == "__CHUNKS__" || current.is_empty() {
                                    *current = String::from("__CHUNKS__");
                                }
                                // Append after the marker
                                if current.starts_with("__CHUNKS__") {
                                    let data_part = current.strip_prefix("__CHUNKS__").unwrap_or("").to_string();
                                    *current = format!("__CHUNKS__{}{}", data_part, v2);
                                }
                                tracing::info!("[udemy-webview] Got chunk {}, accumulated length: {}", value, current.len());
                                break;
                            }
                        }
                        return false;
                    }
                    "done" => {
                        let mut guard = store.lock().unwrap();
                        if let Some(ref mut val) = *guard {
                            if val.starts_with("__CHUNKS__") {
                                let assembled = val.strip_prefix("__CHUNKS__").unwrap_or("").to_string();
                                tracing::info!("[udemy-webview] All chunks assembled, total length: {}", assembled.len());
                                *val = assembled;
                            }
                        }
                        return false;
                    }
                    "error" => {
                        tracing::error!("[udemy-webview] Fetch error: {}", value);
                        *store.lock().unwrap() =
                            Some(format!("{{\"__fetch_error\":\"{}\"}}", value));
                        return false;
                    }
                    _ => {}
                }
            }
            return false;
        }
        true
    })
    .build()
    .map_err(|e| anyhow!("Failed to create API webview: {}", e))?;

    tracing::info!("[udemy-webview] Webview created. Waiting 15s for Cloudflare challenge...");
    tokio::time::sleep(Duration::from_secs(15)).await;
    tracing::info!("[udemy-webview] Wait complete, proceeding with API calls");

    Ok(window)
}

pub async fn webview_get(
    window: &tauri::WebviewWindow,
    url: &str,
    result_store: &Arc<std::sync::Mutex<Option<String>>>,
) -> anyhow::Result<String> {
    *result_store.lock().unwrap() = None;

    tracing::info!("[udemy-webview] webview_get called for: {}", url);

    let js = format!("window.__omniget_fetch('{}')", url);
    window
        .eval(&js)
        .map_err(|e| anyhow!("eval failed: {}", e))?;

    tracing::info!("[udemy-webview] JS eval sent, waiting for response (timeout: 180s)...");

    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(180);
    let mut poll_interval = 100u64;
    loop {
        {
            let guard = result_store.lock().unwrap();
            if let Some(ref data) = *guard {
                // Still receiving chunks, keep waiting
                if data.starts_with("__CHUNKS__") {
                    drop(guard);
                    tokio::time::sleep(Duration::from_millis(poll_interval)).await;
                    continue;
                }
            }
        }

        if let Some(data) = result_store.lock().unwrap().take() {
            if let Some(err_msg) = data.strip_prefix("{\"__fetch_error\":\"") {
                let err_msg = err_msg.trim_end_matches("\"}");
                tracing::error!("[udemy-webview] Fetch returned error after {:.1}s: {}", start.elapsed().as_secs_f64(), err_msg);
                return Err(anyhow!("Fetch error: {}", err_msg));
            }
            tracing::info!("[udemy-webview] Response received after {:.1}s, {} bytes", start.elapsed().as_secs_f64(), data.len());
            return Ok(data);
        }
        if start.elapsed() > timeout {
            tracing::error!("[udemy-webview] TIMEOUT after {}s", timeout.as_secs());
            return Err(anyhow!(
                "Timeout waiting for API response ({}s). The page may be blocked by Cloudflare or the server is slow.",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(poll_interval)).await;
        if poll_interval < 500 {
            poll_interval = (poll_interval * 3 / 2).min(500);
        }
    }
}
