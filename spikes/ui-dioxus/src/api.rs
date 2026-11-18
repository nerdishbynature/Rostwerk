//! Client for the few api-server routes the spike needs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Partner {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub email: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub items: Vec<Partner>,
    pub next_cursor: Option<String>,
}

/// Filter and sort, all evaluated by the API.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    pub q: String,
    /// `customer`, `supplier`, `both` or empty for all.
    pub kind: String,
    /// `created`, `name`, `email` or `kind`, optionally with a leading `-`.
    pub sort: String,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

/// Where the API is. The web build goes through the dev server's proxy.
fn base() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var("ROSTWERK_API").unwrap_or_else(|_| "http://127.0.0.1:18080".into())
    }
}

async fn check(res: reqwest::Response) -> Result<reqwest::Response, String> {
    if res.status().is_success() {
        return Ok(res);
    }
    let status = res.status();
    match res.json::<ApiError>().await {
        Ok(err) => Err(format!("{status}: {}", err.message)),
        Err(_) => Err(status.to_string()),
    }
}

pub async fn list(
    token: &str,
    query: &Query,
    after: Option<&str>,
    limit: usize,
) -> Result<Page, String> {
    let mut params: Vec<(&str, String)> = vec![("limit", limit.to_string())];
    if !query.q.trim().is_empty() {
        params.push(("q", query.q.trim().to_string()));
    }
    if !query.kind.is_empty() {
        params.push(("kind", query.kind.clone()));
    }
    if !query.sort.is_empty() {
        params.push(("sort", query.sort.clone()));
    }
    if let Some(after) = after {
        params.push(("after", after.to_string()));
    }
    let res = reqwest::Client::new()
        .get(format!("{}/v1/partners", base()))
        .query(&params)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    check(res).await?.json().await.map_err(|e| e.to_string())
}

pub async fn rename(token: &str, id: &str, name: &str) -> Result<Partner, String> {
    let res = reqwest::Client::new()
        .patch(format!("{}/v1/partners/{id}", base()))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    check(res).await?.json().await.map_err(|e| e.to_string())
}

pub async fn sleep_ms(ms: u32) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms.into())).await;
}
