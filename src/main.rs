use std::{borrow::Cow, collections::HashMap, env, future, time::Duration};

use futures_util::StreamExt;
use ingress_intel_rs::Intel;
use p0sticcio_bot::{Bot, command, config};
use sea_orm::Database;
use stream_throttle::{ThrottlePool, ThrottleRate, ThrottledStream};
use tgbot::types::{LinkPreviewOptions, ParseMode, SendMessage};
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::error;

#[tokio::main]
async fn main() {
    println!("Started version {}", env!("CARGO_PKG_VERSION"));
    tracing_subscriber::fmt::init();

    let config = config::get().await.expect("Config read failed");

    let conn = Database::connect("postgres://postgres:postgres@postgres/p0sticcio_bot")
        .await
        .expect("Database connection failed");

    let tg_client1 = tgbot::api::Client::new(env::var("BOT_TOKEN1").expect("Missing env var BOT_TOKEN1"))
        .expect("Error building Telegram client for BOT_TOKEN1");
    let tg_client2 = tgbot::api::Client::new(env::var("BOT_TOKEN2").expect("Missing env var BOT_TOKEN2"))
        .expect("Error building Telegram client for BOT_TOKEN2");

    let (global_tx, global_rx) = mpsc::unbounded_channel();
    let config2 = config.clone();
    let conn2 = conn.clone();
    tokio::spawn(async move {
        let tg_client1 = &tg_client1;
        let tg_client2 = &tg_client2;

        // we can send globally only 30 telegram messages per second
        let rate = ThrottleRate::new(30, Duration::from_secs(1));
        let pool = ThrottlePool::new(rate);
        let send_stream = UnboundedReceiverStream::new(global_rx).throttle(pool).for_each_concurrent(
            None,
            |(user_id, msg): (u64, Bot)| async move {
                for i in 0..10 {
                    let client = match msg {
                        Bot::Comm(_) => tg_client1,
                        Bot::Portal(_) => tg_client2,
                    };
                    if let Err(err) = client
                        .execute(
                            SendMessage::new(user_id as i64, msg.get_msg())
                                .with_parse_mode(ParseMode::Markdown)
                                .with_link_preview_options(LinkPreviewOptions::default().with_is_disabled(true)),
                        )
                        .await
                    {
                        error!("Telegram error on retry {}: {}\nuser_id: {}\nmessage: {:?}", i, err, user_id, msg);
                        time::sleep(Duration::from_secs(1)).await;
                    } else {
                        break;
                    }
                }
            },
        );

        tokio::select! {
            _ = send_stream => {
                error!("send stream terminated");
            }
            _ = command::manage(&conn2, &config2, tg_client1) => {
                error!("Bot client1 terminated");
            }
            _ = command::manage(&conn2, &config2, tg_client2) => {
                error!("Bot client2 terminated");
            }
        }
    });

    let senders = config
        .zones
        .iter()
        .flat_map(|z| {
            z.users.keys().map(|id| {
                let (tx, rx) = mpsc::unbounded_channel();
                let global_tx = global_tx.clone();
                let id = *id;
                tokio::spawn(async move {
                    // We can send a single message per telegram chat per second
                    let rate = ThrottleRate::new(1, Duration::from_secs(1));
                    let pool = ThrottlePool::new(rate);
                    UnboundedReceiverStream::new(rx)
                        .throttle(pool)
                        .for_each(|msg| {
                            if let Err(err) = global_tx.send((id, msg)) {
                                error!("User {id} throttler sender error: {err}");
                            }
                            future::ready(())
                        })
                        .await;
                });
                (id, tx)
            })
        })
        .collect::<HashMap<_, _>>();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("Client build failed");
    let username = env::var("USERNAME").ok();
    let password = env::var("PASSWORD").ok();
    let intel = Intel::new(&client, username.map(Cow::Owned), password.map(Cow::Owned));

    if let Ok(cookies) = env::var("COOKIES") {
        intel
            .add_cookies(cookies.split("; ").filter_map(|cookie| {
                let (pos, _) = cookie.match_indices('=').next()?;
                Some((&cookie[0..pos], &cookie[(pos + 1)..]))
            }))
            .await;
    }

    intel.login().await.expect("Intel login failed");

    let (portal_tx, portal_rx) = mpsc::unbounded_channel();

    let conn = &conn;
    let config = &config;
    let intel = &intel;
    let senders = &senders;
    tokio::select! {
        _ = p0sticcio_bot::comm_survey(config, intel, senders, portal_tx.clone()) => {},
        _ = p0sticcio_bot::portal_survey(config, intel, senders) => {},
        _ = p0sticcio_bot::map_survey(conn, config, intel, senders, portal_tx) => {},
        _ = p0sticcio_bot::portal_scanner(conn, intel, portal_rx) => {},
    };
}
