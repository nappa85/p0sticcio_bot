use chrono::Utc;
use smol_str::SmolStr;
use tgbot::types::{EditMessageText, LinkPreviewOptions, ParseMode};
use tokio::sync::oneshot;
use tracing::error;

use super::{Context, Error};

use crate::{PortalOrCoords, entities::Portal, symbols};

pub async fn execute<C>(ctx: &Context<'_, C>, args: impl Iterator<Item = &str>) -> Result<(), Error> {
    // "lat, lon" is split by the whitespace split
    let arg = args.collect::<String>();

    let Some(target) = parse_target(&arg) else {
        ctx.reply("Usage: /scan `intel link`, `lat,lon` or `portal id`").await?;
        return Ok(());
    };

    let (tx, rx) = oneshot::channel();
    // scan anyway, even if the portal is already in the database
    if ctx.portal_tx.send((target, Utc::now().timestamp_millis(), Some(tx))).is_err() {
        ctx.reply("Portal scanner isn't running").await?;
        return Ok(());
    }
    let queued_id = ctx.reply("Scan queued, this message will be updated with the result").await?;

    // the queue can be long during the nightly scan, don't block the commands loop waiting
    let client = ctx.client.clone();
    let chat_id = ctx.chat_id;
    tokio::spawn(async move {
        let text = match rx.await {
            Ok(Ok(Some(portal))) => format!(
                "Scanned {} {}\nSee its changes with `/log {}`",
                symbols::BULLET,
                Portal { name: portal.title.as_str(), address: "maps", lat: portal.lat, lon: portal.lon },
                portal.id
            ),
            Ok(Ok(None)) => String::from("Portal data is already up to date"),
            Ok(Err(err)) => format!("Scan failed: {err}"),
            Err(_) => String::from("Scan aborted"),
        };
        if let Err(err) = client
            .execute(
                EditMessageText::for_chat_message(chat_id, queued_id, text)
                    .with_parse_mode(ParseMode::Markdown)
                    .with_link_preview_options(LinkPreviewOptions::default().with_is_disabled(true)),
            )
            .await
        {
            error!("Scan result edit error: {err}");
        }
    });

    Ok(())
}

/// accepts an intel portal link (`pll` parameter), `lat,lon` or an intel portal id
fn parse_target(arg: &str) -> Option<PortalOrCoords> {
    let coords = match arg.split_once('?') {
        Some((_, query)) => query.split('&').find_map(|param| param.strip_prefix("pll="))?,
        None => arg,
    };
    if let Some((lat, lon)) = coords.split_once(',')
        && let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>())
    {
        return ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
            .then_some(PortalOrCoords::Coords { lat, lon });
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
    use super::*;

    fn coords(arg: &str) -> Option<(f64, f64)> {
        match parse_target(arg)? {
            PortalOrCoords::Coords { lat, lon } => Some((lat, lon)),
            PortalOrCoords::Portal { .. } => None,
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
