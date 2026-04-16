use anyhow::{Context, Result};

use crate::config;

#[derive(serde::Deserialize)]
struct BlacklistResponse {
    is_blacklisted: bool,
}

pub async fn is_blacklisted(config: &config::Config, account_id: &str) -> Result<bool> {
    let url = format!(
        "{}/is_blacklisted?account_id={}",
        config
            .near
            .blacklist_api_url
            .as_ref()
            .context("blacklist_api_url is not configured")?,
        account_id
    );

    let resp: BlacklistResponse = reqwest::get(&url).await?.json().await?;
    Ok(resp.is_blacklisted)
}
