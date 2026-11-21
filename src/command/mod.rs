use std::time::Duration;

use sea_orm::{ConnectionTrait, DbErr, StreamTrait};
use tgbot::{
    api::{Client, ExecuteError},
    types::{
        ChatPeerId, GetUpdates, LinkPreviewOptions, Message, MessageData, ParseMode, ReplyParameters, SendMessage,
        Text, UpdateType, UserPeerId,
    },
};
use tokio::time::sleep;
use tracing::{debug, error};

use crate::{ScanQueue, config::Config};

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
    pub portal_tx: &'a ScanQueue,
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
                SendMessage::new(self.chat_id, (text.into(), ParseMode::Markdown))
                    .with_reply_parameters(ReplyParameters::new(self.message_id))
                    .with_link_preview_options(LinkPreviewOptions::disabled()),
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

pub async fn manage<C>(conn: &C, config: &Config, client: &Client, portal_tx: &ScanQueue)
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tgbot::types::{EditMessageText, Update};

    use super::*;

    fn update(message: Value) -> Update {
        serde_json::from_value(json!({ "update_id": 12345, "message": message })).unwrap()
    }

    pub(crate) fn message(extra: Value) -> Value {
        let mut message = json!({
            "message_id": 678,
            "date": 1_782_864_000,
            "chat": { "id": -100123, "type": "supergroup", "title": "Agents" },
            "from": { "id": 4242, "is_bot": false, "first_name": "Alice", "username": "alice" },
        });
        message.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        message
    }

    #[test]
    fn text_update() {
        let update = update(message(json!({
            "text": "/portals@p0sticcio_bot level>7",
            "entities": [{ "type": "bot_command", "offset": 0, "length": 22 }],
        })));
        assert_eq!(update.id, 12345);
        assert_eq!(update.get_chat_id(), Some(ChatPeerId::from(-100123)));
        assert_eq!(update.get_user().map(|user| user.id), Some(UserPeerId::from(4242)));
        assert_eq!(i64::from(update.get_user().unwrap().id) as u64, 4242);
        let UpdateType::Message(msg) = &update.update_type else { panic!("{update:?}") };
        assert_eq!(msg.id, 678);
        let MessageData::Text(Text { data, .. }) = &msg.data else { panic!("{msg:?}") };
        assert_eq!(data, "/portals@p0sticcio_bot level>7");
    }

    #[test]
    fn location_update() {
        let update = update(message(json!({ "location": { "latitude": 45.5, "longitude": 11.25 } })));
        let UpdateType::Message(msg) = &update.update_type else { panic!("{update:?}") };
        let MessageData::Location(location) = &msg.data else { panic!("{msg:?}") };
        assert_eq!((location.latitude, location.longitude), (45.5, 11.25));
    }

    /// a fake telegram api answering the requests in order with `results`, sends back what it received as (path, json body)
    pub(crate) fn fake_api(results: Vec<Value>) -> (Client, std::sync::mpsc::Receiver<(String, Value)>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for result in results {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap().to_owned();
                let mut len = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = header.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        len = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() };
                tx.send((path, body)).unwrap();
                let response = json!({ "ok": true, "result": result }).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
        });
        (Client::new("0:test").unwrap().with_host(host), rx)
    }

    #[tokio::test]
    async fn requests() {
        let mut sent = message(json!({ "text": "hi" }));
        sent["message_id"] = json!(900);
        let (client, requests) = fake_api(vec![
            sent.clone(),
            json!(true),
            json!([{ "update_id": 12345, "message": message(json!({ "text": "/help" })) }]),
            sent,
        ]);

        // Context::reply
        let (portal_tx, _portal_rx) = crate::scan_queue();
        let ctx = Context {
            conn: &(),
            client: &client,
            portal_tx: &portal_tx,
            chat_id: ChatPeerId::from(-100123),
            user_id: UserPeerId::from(4242),
            message_id: 678,
        };
        assert_eq!(ctx.reply("*hi*").await.unwrap(), 900);
        assert_eq!(
            requests.recv().unwrap(),
            (
                String::from("/bot0:test/sendMessage"),
                json!({
                    "chat_id": -100123,
                    "text": "*hi*",
                    "parse_mode": "Markdown",
                    "reply_parameters": { "message_id": 678 },
                    "link_preview_options": { "is_disabled": true },
                })
            )
        );

        // scan result
        client
            .execute(
                EditMessageText::for_chat_message(ChatPeerId::from(-100123), 679, ("done", ParseMode::Markdown))
                    .with_link_preview_options(LinkPreviewOptions::disabled()),
            )
            .await
            .unwrap();
        assert_eq!(
            requests.recv().unwrap(),
            (
                String::from("/bot0:test/editMessageText"),
                json!({
                    "chat_id": -100123,
                    "message_id": 679,
                    "text": "done",
                    "parse_mode": "Markdown",
                    "link_preview_options": { "is_disabled": true },
                })
            )
        );

        // manage loop
        let updates = client
            .execute(GetUpdates::default().with_timeout(Duration::from_secs(3600)).with_offset(12346))
            .await
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].get_chat_id(), Some(ChatPeerId::from(-100123)));
        assert_eq!(
            requests.recv().unwrap(),
            (String::from("/bot0:test/getUpdates"), json!({ "offset": 12346, "timeout": 3600 }))
        );

        // main notifications
        client
            .execute(
                SendMessage::new(4242_i64, ("hi", ParseMode::Markdown))
                    .with_link_preview_options(LinkPreviewOptions::disabled()),
            )
            .await
            .unwrap();
        assert_eq!(
            requests.recv().unwrap(),
            (
                String::from("/bot0:test/sendMessage"),
                json!({
                    "chat_id": 4242,
                    "text": "hi",
                    "parse_mode": "Markdown",
                    "link_preview_options": { "is_disabled": true },
                })
            )
        );
    }
}
