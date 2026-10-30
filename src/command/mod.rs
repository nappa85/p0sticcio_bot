use std::time::Duration;

use sea_orm::{ConnectionTrait, DbErr, StreamTrait};
use tgbot::{
    api::{Client, ExecuteError},
    types::{
        ChatPeerId, GetUpdates, LinkPreviewOptions, Message, MessageData, ParseMode, ReplyParameters, SendMessage,
        Text, UpdateType, UserPeerId,
    },
};
use tokio::{sync::mpsc, time::sleep};
use tracing::{debug, error};

use crate::{ScanRequest, config::Config};

mod help;
mod location;
mod log;
mod portals;
mod scan;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Telegram error: {0}")]
    Telegram(#[from] ExecuteError),
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
    #[error("Database error: {0} query was {1}")]
    Query(DbErr, String),
}

/// everything a command handler needs about the bot and the message it's answering
pub struct Context<'a, C> {
    pub conn: &'a C,
    pub client: &'a Client,
    pub portal_tx: &'a mpsc::UnboundedSender<ScanRequest>,
    pub chat_id: ChatPeerId,
    pub user_id: UserPeerId,
    pub message_id: i64,
}

impl<C> Context<'_, C> {
    /// replies to the command message as markdown without link previews, returns the sent message id
    pub async fn reply(&self, text: impl Into<String>) -> Result<i64, Error> {
        let message = self
            .client
            .execute(
                SendMessage::new(self.chat_id, text)
                    .with_reply_parameters(ReplyParameters::new(self.message_id))
                    .with_parse_mode(ParseMode::Markdown)
                    .with_link_preview_options(LinkPreviewOptions::default().with_is_disabled(true)),
            )
            .await?;
        Ok(message.id)
    }

    /// replies with every message in order
    pub async fn reply_all(&self, messages: Vec<String>) -> Result<(), Error> {
        for message in messages {
            self.reply(message).await?;
        }
        Ok(())
    }
}

pub async fn manage<C>(conn: &C, config: &Config, client: &Client, portal_tx: &mpsc::UnboundedSender<ScanRequest>)
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

            if let UpdateType::Message(msg) = &update.update_type && let Message { id, data, .. } = msg.as_ref() {
                let ctx = Context { conn, client, portal_tx, chat_id, user_id, message_id: *id };
                match data {
                    MessageData::Text(msg) => {
                        // here we don't retry because it also parses the input
                        if let Err(err) = parse_message(&ctx, msg).await {
                            error!("{err}\nuser_id: {user_id}\nmessage: {msg:?}");
                        }
                    }
                    MessageData::Location(location) => {
                        if let Err(err) = location::set(&ctx, location.latitude, location.longitude).await {
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

async fn parse_message<C>(ctx: &Context<'_, C>, msg: &Text) -> Result<(), Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let Text { data: msg, entities: _ } = msg;

    let mut iter = msg.split_whitespace();
    let res = match iter.next().map(|msg| msg.split_once('@').map(|(pre, _post)| pre).unwrap_or(msg)) {
        Some("/help") => help::execute(ctx).await,
        Some("/portals") => portals::execute(ctx, iter).await,
        Some("/log") => log::execute(ctx, iter).await,
        Some("/scan") => scan::execute(ctx, iter).await,
        _ => return Ok(()),
    };

    if let Err(err) = res {
        ctx.reply(err.to_string()).await?;
    }

    Ok(())
}

/// appends `text` on a new line of the last message, starting a new message when it would exceed telegram 4094 bytes limit
fn push_chunked(messages: &mut Vec<String>, text: String) {
    if let Some(message) = messages.last_mut()
        && message.len() + text.len() + 1 <= 4094
    {
        message.push('\n');
        message.push_str(&text);
    } else {
        messages.push(text);
    }
}
