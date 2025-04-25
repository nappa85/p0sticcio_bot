use std::time::Duration;

use sea_orm::{ConnectionTrait, DbErr, StreamTrait};
use tgbot::{
    api::{Client, ExecuteError},
    types::{
        ChatPeerId, GetUpdates, Message, MessageData, ParseMode, ReplyParameters, SendMessage, Text, UpdateType,
        UserPeerId,
    },
};
use tokio::time::sleep;
use tracing::{debug, error};

use crate::config::Config;

mod help;
mod location;
mod portals;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Telegram error: {0}")]
    Telegram(#[from] ExecuteError),
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
    #[error("Database error: {0} query was {1}")]
    Query(DbErr, String),
}

pub async fn manage<C>(conn: &C, config: &Config, client: &Client)
where
    C: ConnectionTrait + StreamTrait,
{
    let mut offset = -1;
    loop {
        let updates = match client
            .execute(GetUpdates::default().with_timeout(Duration::from_secs(3600)).with_offset(offset + 1))
            .await
        {
            Ok(updates) => updates,
            Err(err) => {
                error!("Telegram get updates error: {err}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        for update in updates {
            let Some(chat_id) = update.get_chat_id() else {
                continue;
            };
            let Some(user) = update.get_user() else {
                continue;
            };
            let user_id = user.id;
            if !config.zones.iter().any(|zone| zone.users.contains_key(&(i64::from(user.id) as u64))) {
                continue;
            }

            if let UpdateType::Message(Message { id, ref data, .. }) = update.update_type {
                match data {
                    MessageData::Text(msg) => {
                        // here we don't retry because it also parses the input
                        if let Err(err) = parse_message(client, conn, chat_id, user_id, id, msg).await {
                            error!("{err}\nuser_id: {user_id}\nmessage: {msg:?}");
                        }
                    }
                    MessageData::Location(location) => {
                        if let Err(err) = location::set(conn, user_id, location.latitude, location.longitude).await {
                            error!("{err}\nuser_id: {user_id}");
                        }
                    }
                    _ => {}
                }
            } else {
                debug!("Ignoring update {update:?}");
            }
            offset = update.id;
        }
    }
}

async fn parse_message<C>(
    client: &Client,
    conn: &C,
    chat_id: ChatPeerId,
    user_id: UserPeerId,
    message_id: i64,
    msg: &Text,
) -> Result<(), Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let Text { data: msg, entities: _ } = msg;

    let mut iter = msg.split_whitespace();
    let res = match iter.next().map(|msg| msg.split_once('@').map(|(pre, _post)| pre).unwrap_or(msg)) {
        Some("/help") => help::execute(client, message_id, chat_id).await,
        Some("/portals") => portals::execute(conn, client, message_id, chat_id, user_id, iter).await,
        _ => return Ok(()),
    };

    if let Err(err) = res {
        client
            .execute(
                SendMessage::new(chat_id, err.to_string())
                    .with_reply_parameters(ReplyParameters::new(message_id))
                    .with_parse_mode(ParseMode::Markdown),
            )
            .await?;
    }

    Ok(())
}
