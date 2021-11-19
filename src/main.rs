use std::{env, time::Duration};

use ingress_intel_rs::{Intel, plexts::Tab};

use once_cell::sync::Lazy;

use serde_json::json;

use tokio::time;

use tracing::{debug, error, info};

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_TOKEN: Lazy<String> = Lazy::new(|| env::var("BOT_TOKEN").expect("Missing env var BOT_TOKEN"));


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", BOT_TOKEN.as_str());

    let mut intel = Intel::build(USERNAME.as_deref(), PASSWORD.as_deref());

    if let Some(cookies) = &*COOKIES {
        for cookie in cookies.split("; ") {
            if let Some((pos, _)) = cookie.match_indices('=').next() {
                intel.add_cookie(&cookie[0..pos], &cookie[(pos + 1)..]);
            }
        }
    }

    let client = reqwest::Client::new();
    let mut interval = time::interval(Duration::from_secs(60));
    let mut last_timestamp = 0;
    loop {
        interval.tick().await;
        if let Ok(res) = intel.get_plexts([45362997, 12066414], [45747158, 12939141], Tab::All).await {
            info!("Got {} plexts", res.result.len());
            if res.result.is_empty() {
                continue;
            }

            for (_id, time, plext) in res.result.iter().rev() {
                if last_timestamp > 0 && *time > last_timestamp {
                    client.post(&url)
                        .header("Content-Type", "application/json")
                        .json(&json!({
                            "chat_id": -532100731,
                            "text": plext.plext.text.as_str(),
                        }))
                        .send()
                        .await
                        .map_err(|e| error!("{}", e))
                        .ok();
                }
                else {
                    debug!("plext time {} and last_timestamp {}", time, last_timestamp);
                }
            }

            last_timestamp = res.result.first().map(|(_, t, _)| *t).unwrap_or_default();
        }
    }
}
