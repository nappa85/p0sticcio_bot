use std::{collections::HashMap, env, future, time::Duration};

use chrono::Utc;

use futures_util::{Stream, StreamExt, stream::unfold};

use ingress_intel_rs::{Intel, plexts::Tab};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use serde_json::json;

use stream_throttle::{ThrottlePool, ThrottleRate, ThrottledStream};

use tokio::{time, sync::mpsc};

use tracing::{debug, error, info};

mod config;
mod entities;

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_TOKEN: Lazy<String> = Lazy::new(|| env::var("BOT_TOKEN").expect("Missing env var BOT_TOKEN"));

fn make_stream<T>(rx: mpsc::UnboundedReceiver<T>) -> impl Stream<Item=T> {
    unfold(rx, |mut rx| async {
        let next = rx.recv().await?;
        Some((next, rx))
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt::init();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", BOT_TOKEN.as_str());
    let config = config::get().await.unwrap();

    let (global_tx, global_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // we can send globally only 30 telegram messages per second
        let rate = ThrottleRate::new(30, Duration::from_secs(1));
        let pool = ThrottlePool::new(rate);
        let client = reqwest::Client::new();
        let c = &client;
        let u = &url;
        make_stream(global_rx).throttle(pool).for_each_concurrent(None, |(user_id, msg): (u64, String)| async move {
            loop {
                let res = c.post(u)
                    .header("Content-Type", "application/json")
                    .json(&json!({
                        "chat_id": user_id,
                        "text": msg.as_str(),
                        "parse_mode": "HTML",
                        "disable_web_page_preview": true
                    }))
                    .send()
                    .await
                    .map_err(|e| error!("Telegram error: {}\nuser_id: {}\nmessage: {}", e, user_id, msg));
                if let Ok(res) = res {
                    if res.status().is_success() {
                        break;
                    }

                    error!("Telegram error: {:?}\nuser_id: {}\nmessage: {}", res.text().await, user_id, msg);
                }
                time::sleep(Duration::from_secs(1)).await;
            }
        }).await;
    });

    let senders = config.zones.iter().map(|z| z.users.iter().map(|(id, _)| {
        let (tx, rx) = mpsc::unbounded_channel();
        let global_tx = global_tx.clone();
        let id = *id;
        tokio::spawn(async move {
            // We can send a single message per telegram chat per second
            let rate = ThrottleRate::new(1, Duration::from_secs(1));
            let pool = ThrottlePool::new(rate);
            make_stream(rx).throttle(pool).for_each(|msg| {
                global_tx.send((id, msg))
                    .map_err(|e| error!("Sender error: {}", e))
                    .ok();
                future::ready(())
            }).await;
        });
        (id, tx)
    })).flatten().collect::<HashMap<_, _>>();

    let mut intel = Intel::build(USERNAME.as_deref(), PASSWORD.as_deref());

    if let Some(cookies) = &*COOKIES {
        for cookie in cookies.split("; ") {
            if let Some((pos, _)) = cookie.match_indices('=').next() {
                intel.add_cookie(&cookie[0..pos], &cookie[(pos + 1)..]);
            }
        }
    }

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
                                senders[id].send(msg.to_string())
                                    .map_err(|e| error!("Sender error: {}", e))
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
