use std::{env, time::Duration};

use chrono::Utc;

use ingress_intel_rs::{Intel, plexts::Tab};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use serde_json::json;

use tokio::time;

use tracing::{debug, error, info};

mod config;
mod entities;

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_TOKEN: Lazy<String> = Lazy::new(|| env::var("BOT_TOKEN").expect("Missing env var BOT_TOKEN"));

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt::init();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", BOT_TOKEN.as_str());
    let config = config::get().await.unwrap();

    let mut intel = Intel::build(USERNAME.as_deref(), PASSWORD.as_deref());

    if let Some(cookies) = &*COOKIES {
        for cookie in cookies.split("; ") {
            if let Some((pos, _)) = cookie.match_indices('=').next() {
                intel.add_cookie(&cookie[0..pos], &cookie[(pos + 1)..]);
            }
        }
    }

    let client = reqwest::Client::new();
    let mut interval = time::interval(Duration::from_secs(30));
    let mut last_timestamp = vec![Utc::now().timestamp_millis(); config.zones.len()];
    let mut sent_cache: LruCache<String, ()> = LruCache::with_expiry_duration(Duration::from_secs(120));//2 minutes cache
    loop {
        for (index, zone) in config.zones.iter().enumerate() {
            interval.tick().await;
            if let Ok(res) = intel.get_plexts(zone.from, zone.to, Tab::All, Some(last_timestamp[index]), None).await {
                info!("Got {} plexts", res.result.len());
                if res.result.is_empty() {
                    continue;
                }

                let msgs = res.result.iter().rev().filter_map(|(_id, time, plext)| {
                    if *time > last_timestamp[index] {
                        let msg_type = entities::PlextType::from(plext.plext.markup.as_slice());
                        entities::Plext::try_from((msg_type, &plext.plext)).ok()
                    }
                    else {
                        debug!("plext time {} and last_timestamp {}", time, last_timestamp[index]);
                        None
                    }
                }).collect::<Vec<_>>();
                for msg in msgs.iter().filter(|m| !m.has_duplicates(&msgs)) {
                    if sent_cache.notify_insert(msg.to_string(), ()).0.is_none() {
                        for (id, filter) in &zone.users {
                            if filter.apply(msg) {
                                client.post(&url)
                                    .header("Content-Type", "application/json")
                                    .json(&json!({
                                        "chat_id": id,
                                        "text": msg.to_string(),
                                        "parse_mode": "HTML",
                                        "disable_web_page_preview": true
                                    }))
                                    .send()
                                    .await
                                    .map_err(|e| error!("{}", e))
                                    .ok();
                            }
                        }
                    }
                    else {
                        debug!("Blocked duplicate entry {:?}", msg);
                    }
                }

                if let Some((_, t, _)) = res.result.first() {
                    last_timestamp[index] = *t;
                }
            }
        }
    }
}
