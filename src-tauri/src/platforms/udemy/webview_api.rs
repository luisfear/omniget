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
    .devtools(true)
    .initialization_script(
        r#"
        // === OMNIGET DEBUG CONSOLE ===
        console.log('[omniget] initialization script loaded');
        console.log('[omniget] current URL:', window.location.href);
        console.log('[omniget] document title:', document.title);
        console.log('[omniget] user agent:', navigator.userAgent);

        // Log all page navigations
        var _omniget_observer = new MutationObserver(function() {
            console.log('[omniget] DOM changed - title:', document.title, 'url:', window.location.href);
        });
        _omniget_observer.observe(document, { childList: true, subtree: true });

        // Log when page fully loads
        window.addEventListener('load', function() {
            console.log('[omniget] page loaded:', window.location.href);
            console.log('[omniget] page title:', document.title);
            console.log('[omniget] cookies available:', document.cookie ? 'yes' : 'no');

            // Detect Cloudflare challenge
            if (document.title.includes('Just a moment') || 
                document.title.includes('Attention Required') ||
                document.title.includes('Checking')) {
                console.warn('[omniget] CLOUDFLARE CHALLENGE DETECTED! Please solve it manually.');
            }
        });

        // Log all fetch/XHR requests
        var _origFetch = window.fetch;
        window.fetch = function() {
            var url = arguments[0];
            if (typeof url === 'string') {
                console.log('[omniget] fetch request to:', url);
            } else if (url && url.url) {
                console.log('[omniget] fetch request to:', url.url);
            }
            return _origFetch.apply(this, arguments).then(function(response) {
                console.log('[omniget] fetch response:', response.status, response.statusText, 'from:', response.url);
                return response;
            }).catch(function(err) {
                console.error('[omniget] fetch error:', err.message);
                throw err;
            });
        };

        // The actual fetch function for curriculum
        window.__omniget_fetch = function(url) {
            console.log('[omniget] === CURRICULUM FETCH START ===');
            console.log('[omniget] fetching:', url);
            console.log('[omniget] current page:', window.location.href);
            console.log('[omniget] page title:', document.title);

            _origFetch(url, {
                credentials: 'include',
                headers: {
                    'Accept': 'application/json, text/plain, */*',
                    'X-Requested-With': 'XMLHttpRequest'
                }
            })
            .then(function(r) {
                console.log('[omniget] API response status:', r.status, r.statusText);
                console.log('[omniget] API response headers content-type:', r.headers.get('content-type'));
                return r.text();
            })
            .then(function(text) {
                console.log('[omniget] API response length:', text.length, 'bytes');
                console.log('[omniget] API response preview:', text.substring(0, 200));
                if (text.length === 0) {
                    console.error('[omniget] EMPTY RESPONSE - likely blocked by Cloudflare');
                }
                window.location.href = 'https://omniget-api-result.local/?data=' + encodeURIComponent(text);
            })
            .catch(function(err) {
                console.error('[omniget] === FETCH FAILED ===');
                console.error('[omniget] error:', err.message);
                window.location.href = 'https://omniget-api-result.local/?error=' + encodeURIComponent(err.message);
            });
        };
        "#,
    )
    .on_navigation(move |url| {
        tracing::info!("[udemy-webview] Navigation to: {}", url.as_str());

        if url.host_str() == Some("omniget-api-result.local") {
            tracing::info!("[udemy-webview] Got API result callback");
            for (key, value) in url.query_pairs() {
                if key == "data" {
                    let preview: String = value.chars().take(200).collect();
                    tracing::info!("[udemy-webview] Data received, length: {}, preview: {}", value.len(), preview);
                    *store.lock().unwrap() = Some(value.to_string());
                    return false;
                }
                if key == "error" {
                    tracing::error!("[udemy-webview] Fetch error received: {}", value);
                    *store.lock().unwrap() =
                        Some(format!("{{\"__fetch_error\":\"{}\"}}", value));
                    return false;
                }
            }
            return false;
        }
        true
    })
    .build()
    .map_err(|e| anyhow!("Failed to create API webview: {}", e))?;

    tracing::info!("[udemy-webview] Webview created. Waiting 15s for Cloudflare challenge...");

    // Wait 15 seconds so the user can pass any Cloudflare challenge
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
