use tgbot::{
    api::Client,
    types::{ChatPeerId, ParseMode, ReplyParameters, SendMessage},
};

use super::Error;

const HELP_MESSAGE: &str = r#"[P0sticcio Bot](https://github.com/nappa85/p0sticcio_bot/)

/help - this message
/portals `[filters]` - shows all portals matching given \[filters]

`[filters]` can be any number of those:
● `{field}{comparison}{value}` - comparison between a field and a value, where comparisons can be
 ► `=` - equals
 ► `<>` - not equals
 ► `<` - minor than
 ► `<=` - minor or equal than
 ► `>` - major than
 ► `>=` - major or equal than
 ► `~` - like (use % as wildcard)
● `and([filters])` - creates a subgroup where comparison operations are matched by and (default)
● `or([filters])` - creates a subgroup where comparison operations are matched by or
● `{N}` - where N is the maximum number of portals you want to see
● `name` - sort by portal name (default)
● `level` - sort by portal level (higher first)
● `older` - sort by portal age (latest faction change, older first)
● `nearer` - sort by distance from your position (nearest first)

Available fields are:
● `title` - portal title, string
● `revision` - portal revision, integer
● `timestamp` - revision timestamp, integer
● `faction` - portal faction, string
 ► `N` - Neutral
 ► `E` - Enlightened
 ► `R` - Resistance
 ► `M` - Machina
● `level` - portal level, integer
● `health` - portal healt percent, integer
● `res_count` - portal resonators count, integer
● `owner` - portal owner, string
● `reso_owner` - resonator owner, string
● `reso_level` - resonator level, integer
● `reso_energy` - resonator energy, integer
● `mod_owner` - mod owner, string
● `mod_name` - mod name, string
● `mod_rarity` - mod rarity, string

Example
/portals owner="Niantic" older 10
Retrieves the 10 oldest portals owned by player Niantic

Simply send a position to the bot to use the `nearer` sorter
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
