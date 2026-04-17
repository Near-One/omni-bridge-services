use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use near_sdk::AccountId;
use reqwest::{Client, Url};

use crate::config;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlacklistResponse {
    is_blacklisted: bool,
}

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build blacklist reqwest client")
    })
}

fn build_url(base_url: &str, account_id: &AccountId) -> Result<Url> {
    Url::parse(base_url)
        .context("Failed to parse blacklist_api_url")?
        .join(account_id.as_str())
        .context("Failed to build blacklist API URL")
}

pub async fn is_blacklisted(config: &config::Config, account_id: &AccountId) -> Result<bool> {
    let base_url = config
        .near
        .blacklist_api_url
        .as_ref()
        .context("blacklist_api_url is not configured")?;

    let url = build_url(base_url, account_id)?;

    let resp: BlacklistResponse = client().get(url).send().await?.json().await?;
    Ok(resp.is_blacklisted)
}
