use std::{env, time::Duration};

use chrono::Utc;

use ingress_intel_rs::{Intel, plexts::{Markup, Tab}};

use lru_time_cache::LruCache;

use once_cell::sync::Lazy;

use serde_json::json;

use tokio::time;

use tracing::{debug, error, info};

static USERNAME: Lazy<Option<String>> = Lazy::new(|| env::var("USERNAME").ok());
static PASSWORD: Lazy<Option<String>> = Lazy::new(|| env::var("PASSWORD").ok());
static COOKIES: Lazy<Option<String>> = Lazy::new(|| env::var("COOKIES").ok());
static BOT_TOKEN: Lazy<String> = Lazy::new(|| env::var("BOT_TOKEN").expect("Missing env var BOT_TOKEN"));

fn parse_markup(markup: &Markup) -> String {
    match markup.0.as_str() {
        "PORTAL" => {
            let coords = format!("{},{}", markup.1.lat_e6.unwrap_or_default() as f64 / 1000000.0, markup.1.lng_e6.unwrap_or_default() as f64 / 1000000.0);
            format!(
                "<a href=\"https://intel.ingress.com/intel?pll={}\">{}</a> (<a href=\"https://maps.google.it/maps/?q={}\">{}</a>)",
                coords,
                markup.1.name.as_deref().unwrap_or_default(),
                coords,
                markup.1.address.as_deref().unwrap_or_default()
            )
        },
        "PLAYER" => {
            match markup.1.team.as_deref() {
                Some("ENLIGHTENED") => format!("{}{}", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x9F, 0xA2]) }, markup.1.plain),
                Some("RESISTANCE") => format!("{}{}", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x94, 0xB5]) }, markup.1.plain),
                _ => markup.1.plain.clone(),
            }
        },
        "TEXT" => {
            match markup.1.plain.as_str() {
                " captured " => format!(" {}captured ", unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9B, 0xB3]) }),//flag
                " created a Control Field @" => format!(" {}created a Control Field @", unsafe { String::from_utf8_unchecked(vec![0xE2, 0x96, 0xB3]) }),//triangle
                " deployed a Resonator on " => format!(" {}deployed a Resonator on ", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0xA7, 0xB1]) }),//bricks
                " destroyed a Resonator on " => format!(" {}destroyed a Resonator on ", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x92, 0xA5]) }),//explosion
                " destroyed the Link " => format!(" {} destroyed the Link", unsafe { String::from_utf8_unchecked(vec![0xE2, 0x9C, 0x82]) }),//scissors
                " linked " => format!(" {}linked ", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x94, 0x97]) }),//chain
                "Drone returned to Agent by " => format!("{}Drone returned to Agent by ", unsafe { String::from_utf8_unchecked(vec![0xF0, 0x9F, 0x9B, 0xB8]) }),//ufo
                _ => markup.1.plain.clone(),
            }
        },
        _ => markup.1.plain.clone(),
    }
}

#[tokio::main]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
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
    let mut last_timestamp = Utc::now().timestamp_millis();
    let mut sent_cache: LruCache<String, ()> = LruCache::with_expiry_duration(Duration::from_secs(120));//2 minutes cache
    loop {
        interval.tick().await;
        if let Ok(res) = intel.get_plexts([45362997, 12066414], [45747158, 12939141], Tab::All, Some(last_timestamp), None).await {
            info!("Got {} plexts", res.result.len());
            if res.result.is_empty() {
                continue;
            }

            for (_id, time, plext) in res.result.iter().rev() {
                if *time > last_timestamp {
                    let msg = plext.plext.markup.iter().map(parse_markup).collect::<Vec<_>>().join("");
                    if sent_cache.notify_insert(msg.clone(), ()).0.is_none() {
                        client.post(&url)
                            .header("Content-Type", "application/json")
                            .json(&json!({
                                "chat_id": -532100731,
                                "text": msg,
                                "parse_mode": "HTML"
                            }))
                            .send()
                            .await
                            .map_err(|e| error!("{}", e))
                            .ok();
                    }
                    else {
                        debug!("Blocked duplicate entry {:?}", plext);
                    }
                }
                else {
                    debug!("plext time {} and last_timestamp {}", time, last_timestamp);
                }
            }

            last_timestamp = res.result.first().map(|(_, t, _)| *t).unwrap_or_default();
        }
    }
}
