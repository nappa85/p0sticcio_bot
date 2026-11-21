use std::time::Duration;

use chrono::Utc;
use smol_str::SmolStr;
use tgbot::{
    api::Client,
    types::{ChatPeerId, EditMessageText, LinkPreviewOptions, ParseMode, ReplyParameters, SendMessage},
};
use tokio::{sync::mpsc, time::Instant};
use tracing::error;

use super::{Context, Error};

use crate::{PortalOrCoords, ScanUpdate, ScannedPortal, entities::Portal, symbols};

/// minimum time between progress edits, telegram limits edits too
const PROGRESS_INTERVAL: Duration = Duration::from_secs(3);

pub async fn execute<C>(ctx: &Context<'_, C>, args: impl Iterator<Item = &str>) -> Result<(), Error> {
    // "lat, lon" is split by the whitespace split
    let arg = args.collect::<String>();

    let Some(target) = parse_target(&arg) else {
        ctx.reply("Usage: /scan `portal id`, or `intel link` or `lat,lon` to scan every portal around").await?;
        return Ok(());
    };
    let what = if matches!(target, PortalOrCoords::Area { .. }) { "Area scan" } else { "Scan" };

    let (tx, rx) = mpsc::unbounded_channel();
    // scan anyway, even if the portal is already in the database
    let Ok(position) = ctx.portal_tx.send((target, Utc::now().timestamp_millis(), Some(tx))) else {
        ctx.reply("Portal scanner isn't running").await?;
        return Ok(());
    };
    let queued_id = ctx
        .reply(format!("{what} queued at position {position}, this message will be updated with the result"))
        .await?;

    // the queue can be long during the nightly scan, don't block the commands loop waiting
    let client = ctx.client.clone();
    let chat_id = ctx.chat_id;
    let message_id = ctx.message_id;
    tokio::spawn(async move {
        let messages = follow(rx, |progress| {
            let client = client.clone();
            async move { edit(&client, chat_id, queued_id, progress).await }
        })
        .await;
        let mut messages = messages.into_iter();
        if let Some(first) = messages.next() {
            edit(&client, chat_id, queued_id, first).await;
        }
        // results that don't fit the queued message
        for text in messages {
            if let Err(err) = client
                .execute(
                    SendMessage::new(chat_id, (text, ParseMode::Markdown))
                        .with_reply_parameters(ReplyParameters::new(message_id))
                        .with_link_preview_options(LinkPreviewOptions::disabled()),
                )
                .await
            {
                error!("Scan result send error: {err}");
            }
        }
    });

    Ok(())
}

async fn edit(client: &Client, chat_id: ChatPeerId, message_id: i64, text: String) {
    if let Err(err) = client
        .execute(
            EditMessageText::for_chat_message(chat_id, message_id, (text, ParseMode::Markdown))
                .with_link_preview_options(LinkPreviewOptions::disabled()),
        )
        .await
    {
        error!("Scan message edit error: {err}");
    }
}

/// waits for the scan updates, reporting the progress of area scans, returns the result messages
async fn follow<F>(mut rx: mpsc::UnboundedReceiver<ScanUpdate>, mut progress: impl FnMut(String) -> F) -> Vec<String>
where
    F: Future<Output = ()>,
{
    let mut total = None;
    let mut lines = vec![];
    let (mut stored, mut up_to_date, mut failed) = (0, 0, 0);
    let mut last_progress: Option<Instant> = None;
    // the scanner keeps the sender until it's done waiting between scans, don't wait for it to close
    while let Some(update) = rx.recv().await {
        match update {
            ScanUpdate::Portal(outcome) => {
                return vec![match outcome {
                    Ok(Some(portal)) => {
                        format!("Scanned {}\nSee its changes with `/log {}`", line(&portal), portal.id)
                    }
                    Ok(None) => String::from("Portal data is already up to date"),
                    Err(err) => format!("Scan failed: {}", code(&err)),
                }];
            }
            ScanUpdate::Area(Err(err)) => return vec![format!("Area scan failed: {}", code(&err))],
            ScanUpdate::Area(Ok(0)) => return vec![String::from("No portals found in the area")],
            ScanUpdate::Area(Ok(n)) => {
                total = Some(n);
                last_progress = Some(Instant::now());
                progress(format!("Scanning {n} portals in the area, nearest first: 0/{n} done")).await;
            }
            ScanUpdate::AreaPortal { portal, outcome } => {
                lines.push(match outcome {
                    Ok(Some(scanned)) => {
                        stored += 1;
                        format!("{} `/log {}`", line(&scanned), scanned.id)
                    }
                    Ok(None) => {
                        up_to_date += 1;
                        format!("{} up to date", line(&portal))
                    }
                    Err(err) => {
                        failed += 1;
                        format!("{} failed: {}", line(&portal), code(&err))
                    }
                });
                let Some(n) = total else { continue };
                if lines.len() >= n {
                    break;
                }
                if last_progress.is_none_or(|last| last.elapsed() >= PROGRESS_INTERVAL) {
                    last_progress = Some(Instant::now());
                    progress(format!("Scanning {n} portals in the area, nearest first: {}/{n} done", lines.len()))
                        .await;
                }
            }
        }
    }

    let Some(n) = total else {
        return vec![String::from("Scan aborted")];
    };
    let header = if lines.len() < n {
        format!("Area scan aborted after {}/{n} portals", lines.len())
    } else {
        format!("Scanned {n} portals in the area: {stored} updated, {up_to_date} already up to date, {failed} failed")
    };
    let mut messages = vec![header];
    for line in lines {
        super::push_chunked(&mut messages, line);
    }
    messages
}

fn line(portal: &ScannedPortal) -> String {
    format!(
        "{} {}",
        symbols::BULLET,
        Portal { name: portal.title.as_str(), address: "maps", lat: portal.lat, lon: portal.lon }
    )
}

/// markdown code span, errors can contain markdown characters
fn code(text: &str) -> String {
    format!("`{}`", text.replace('`', ""))
}

/// accepts an intel portal id, or an intel link (`pll` parameter) or `lat,lon` for the area around it
fn parse_target(arg: &str) -> Option<PortalOrCoords> {
    let coords = match arg.split_once('?') {
        Some((_, query)) => query.split('&').find_map(|param| param.strip_prefix("pll="))?,
        None => arg,
    };
    if let Some((lat, lon)) = coords.split_once(',')
        && let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>())
    {
        return ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
            .then_some(PortalOrCoords::Area { lat, lon });
    }
    is_portal_id(arg).then(|| PortalOrCoords::Portal { id: SmolStr::new(arg) })
}

/// intel portal ids look like `0f96e13aeac740b8bc88f8bc14011e71.16`
fn is_portal_id(arg: &str) -> bool {
    arg.split_once('.').is_some_and(|(guid, suffix)| {
        guid.len() == 32
            && guid.chars().all(|c| c.is_ascii_hexdigit())
            && !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tgbot::types::UserPeerId;

    use super::*;
    use crate::command::tests::{fake_api, message};

    fn portal(n: usize) -> ScannedPortal {
        ScannedPortal {
            id: SmolStr::new(format!("{n:032x}.16")),
            title: SmolStr::new(format!("Portal {n}")),
            lat: 45.0 + n as f64 / 1000.0,
            lon: 11.0,
        }
    }

    /// runs `follow` over the updates, the sender is kept open unless `close`
    async fn run(updates: Vec<ScanUpdate>, close: bool) -> (Vec<String>, Vec<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        for update in updates {
            tx.send(update).unwrap();
        }
        if close {
            drop(tx);
        } else {
            // the scanner may still be sleeping when the last update is sent
            std::mem::forget(tx);
        }
        let mut progress = vec![];
        let messages = tokio::time::timeout(
            Duration::from_secs(5),
            follow(rx, |text| {
                progress.push(text);
                std::future::ready(())
            }),
        )
        .await
        .expect("follow waited for the sender to close");
        (messages, progress)
    }

    #[tokio::test]
    async fn single_portal() {
        let (messages, progress) = run(vec![ScanUpdate::Portal(Ok(Some(portal(1))))], false).await;
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("Scanned ● [Portal 1]("), "{messages:?}");
        assert!(messages[0].ends_with(&format!("See its changes with `/log {}`", portal(1).id)), "{messages:?}");
        assert!(progress.is_empty());

        let (messages, _) = run(vec![ScanUpdate::Portal(Ok(None))], false).await;
        assert_eq!(messages, ["Portal data is already up to date"]);
        let (messages, _) = run(vec![ScanUpdate::Portal(Err(String::from("Intel `error`")))], false).await;
        assert_eq!(messages, ["Scan failed: `Intel error`"]);
        let (messages, _) = run(vec![], true).await;
        assert_eq!(messages, ["Scan aborted"]);
    }

    #[tokio::test]
    async fn area() {
        let (messages, progress) = run(
            vec![
                ScanUpdate::Area(Ok(3)),
                ScanUpdate::AreaPortal { portal: portal(1), outcome: Ok(Some(portal(1))) },
                ScanUpdate::AreaPortal { portal: portal(2), outcome: Ok(None) },
                ScanUpdate::AreaPortal { portal: portal(3), outcome: Err(String::from("Database error: x_y")) },
            ],
            false,
        )
        .await;
        // the following ones are throttled
        assert_eq!(progress, ["Scanning 3 portals in the area, nearest first: 0/3 done"]);
        assert_eq!(messages.len(), 1, "{messages:?}");
        let lines = messages[0].lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0], "Scanned 3 portals in the area: 1 updated, 1 already up to date, 1 failed");
        assert!(lines[1].starts_with("● [Portal 1](") && lines[1].ends_with(&format!(" `/log {}`", portal(1).id)));
        assert!(lines[2].starts_with("● [Portal 2](") && lines[2].ends_with(") up to date"));
        assert!(lines[3].starts_with("● [Portal 3](") && lines[3].ends_with(") failed: `Database error: x_y`"));

        let (messages, _) =
            run(vec![ScanUpdate::Area(Ok(3)), ScanUpdate::AreaPortal { portal: portal(1), outcome: Ok(None) }], true)
                .await;
        assert!(messages[0].starts_with("Area scan aborted after 1/3 portals\n● [Portal 1]("), "{messages:?}");

        let (messages, _) = run(vec![ScanUpdate::Area(Ok(0))], false).await;
        assert_eq!(messages, ["No portals found in the area"]);
        let (messages, _) = run(vec![ScanUpdate::Area(Err(String::from("no Intel")))], false).await;
        assert_eq!(messages, ["Area scan failed: `no Intel`"]);
    }

    #[tokio::test]
    async fn big_area_is_chunked() {
        let mut updates = vec![ScanUpdate::Area(Ok(200))];
        updates.extend((0..200).map(|n| ScanUpdate::AreaPortal { portal: portal(n), outcome: Ok(Some(portal(n))) }));
        let (messages, _) = run(updates, false).await;
        assert!(messages.len() > 1);
        assert!(messages.iter().all(|message| message.len() <= 4094));
        assert_eq!(messages.iter().flat_map(|message| message.lines()).count(), 201);
    }

    /// the whole command, playing the scanner against a fake telegram
    #[tokio::test(flavor = "multi_thread")]
    async fn command() {
        let mut queued = message(json!({ "text": "queued" }));
        queued["message_id"] = json!(900);
        let (client, requests) = fake_api(vec![queued, json!(true), json!(true)]);
        let (portal_tx, mut portal_rx) = crate::scan_queue();
        // two requests are already waiting
        for _ in 0..2 {
            portal_tx.send((PortalOrCoords::Portal { id: SmolStr::new("x") }, 0, None)).unwrap();
        }
        let ctx = Context {
            conn: &(),
            client: &client,
            portal_tx: &portal_tx,
            chat_id: ChatPeerId::from(-100123),
            user_id: UserPeerId::from(4242),
            message_id: 678,
        };
        execute(&ctx, "https://intel.ingress.com/intel?pll=45.855956,11.824894".split_whitespace()).await.unwrap();
        let recv = || requests.recv_timeout(Duration::from_secs(5)).unwrap();

        let (path, body) = recv();
        assert_eq!(path, "/bot0:test/sendMessage");
        assert_eq!(body["text"], "Area scan queued at position 3, this message will be updated with the result");

        portal_rx.recv().await.unwrap();
        portal_rx.recv().await.unwrap();
        let (PortalOrCoords::Area { lat, lon }, _, Some(reply)) = portal_rx.recv().await.unwrap() else { panic!() };
        assert_eq!((lat, lon), (45.855956, 11.824894));
        reply.send(ScanUpdate::Area(Ok(1))).unwrap();

        let (path, body) = recv();
        assert_eq!(path, "/bot0:test/editMessageText");
        assert_eq!(body["message_id"], 900);
        assert_eq!(body["text"], "Scanning 1 portals in the area, nearest first: 0/1 done");

        reply.send(ScanUpdate::AreaPortal { portal: portal(1), outcome: Ok(Some(portal(1))) }).unwrap();
        let (path, body) = recv();
        assert_eq!(path, "/bot0:test/editMessageText");
        assert_eq!(body["message_id"], 900);
        let text = body["text"].as_str().unwrap();
        assert!(
            text.starts_with("Scanned 1 portals in the area: 1 updated, 0 already up to date, 0 failed\n● [Portal 1](")
        );
    }

    fn coords(arg: &str) -> Option<(f64, f64)> {
        match parse_target(arg)? {
            PortalOrCoords::Area { lat, lon } => Some((lat, lon)),
            PortalOrCoords::Portal { .. } | PortalOrCoords::Coords { .. } | PortalOrCoords::Watch { .. } => None,
        }
    }

    #[test]
    fn targets() {
        assert_eq!(coords("https://intel.ingress.com/intel?pll=45.855956,11.824894"), Some((45.855956, 11.824894)));
        assert_eq!(coords("https://intel.ingress.com/intel?ll=1,2&z=17&pll=46.508889,11.828056"), Some((46.508889, 11.828056)));
        assert_eq!(coords("46.508889,11.828056"), Some((46.508889, 11.828056)));
        assert!(matches!(
            parse_target("0f96e13aeac740b8bc88f8bc14011e71.16"),
            Some(PortalOrCoords::Portal { id }) if id == "0f96e13aeac740b8bc88f8bc14011e71.16"
        ));
        assert!(parse_target("https://intel.ingress.com/intel?ll=1,2").is_none());
        assert!(parse_target("Croce di Punta Frontale").is_none());
        assert!(parse_target("95,11").is_none());
    }
}
