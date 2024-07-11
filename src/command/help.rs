use tgbot::{
    api::Client,
    types::{ChatPeerId, ParseMode, ReplyParameters, SendMessage},
};

use super::Error;

const HELP_MESSAGE: &str = r#"[P0sticcio Bot](https://github.com/nappa85/p0sticcio_bot/)

/help - this message
/owner {player} \[filters] - shows all portals owned by that player
/portals {player} \[filters] - shows all portals owned or with at least a mod or a resonator from that player

\[filters] could be any number of those:
● lvl=N - where N is the minimum portal level
● N - where N is the maximum number of portals you want to see
● name - sort by portal name (default)
● level - sort by portal level (higher first)
● older - sort by portal age (latest faction change, older first)

Example
/owner Niantic older 10
Retrieves the 10 oldest portals owned by player Niantic
"#;

pub async fn execute(client: &Client, message_id: i64, chat_id: ChatPeerId) -> Result<(), Error> {
    client
        .execute(
            SendMessage::new(chat_id, HELP_MESSAGE)
                .with_reply_parameters(ReplyParameters::new(message_id))
                .with_parse_mode(ParseMode::Markdown),
        )
        .await?;

    Ok(())
}
