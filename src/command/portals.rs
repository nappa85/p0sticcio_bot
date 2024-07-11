use std::{future, iter, str::FromStr};

use futures_util::{StreamExt, TryStreamExt};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement, StreamTrait, Value};
use tgbot::{
    api::Client,
    types::{ChatPeerId, LinkPreviewOptions, ParseMode, ReplyParameters, SendMessage},
};

use super::Error;

use crate::{database::portal, entities::generate_player_link, symbols};

#[derive(Copy, Clone, Debug, Default)]
enum Sort {
    #[default]
    Name,
    Level,
    Older,
}

impl FromStr for Sort {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("name") {
            Ok(Sort::Name)
        } else if s.eq_ignore_ascii_case("level") {
            Ok(Sort::Level)
        } else if s.eq_ignore_ascii_case("older") {
            Ok(Sort::Older)
        } else {
            Err(())
        }
    }
}

impl Sort {
    fn is_older(&self) -> bool {
        matches!(self, Sort::Older)
    }

    fn sort(&self) -> &'static str {
        match self {
            Sort::Name => "p.title ASC",
            Sort::Level => "pr.level DESC",
            Sort::Older => "days DESC",
        }
    }
}

#[derive(Debug, Default)]
struct Filters<'a> {
    owner: bool,
    player: &'a str,
    min_level: Option<u8>,
    sort: Option<Sort>,
    limit: Option<u8>,
}

impl<'a> Filters<'a> {
    fn new(owner: bool, mut filters: impl Iterator<Item = &'a str>) -> Result<Self, String> {
        let Some(player) = filters.next() else {
            return Err(String::from("Error: no player name"));
        };

        let (min_level, sort, limit) =
            filters.try_fold((None, None, None), |(mut min_level, mut sort, mut limit), part| {
                if let Some(lvl) = part.strip_prefix("lvl=") {
                    min_level = Some(lvl.parse().map_err(|err| format!("Invalid min level: {err}"))?);
                } else if let Ok(s) = Sort::from_str(part) {
                    sort = Some(s);
                } else if let Ok(l) = part.parse() {
                    limit = Some(l);
                } else {
                    return Err(format!("Unrecognized filter \"{part}\""));
                }
                Ok((min_level, sort, limit))
            })?;

        Ok(Self { owner, player, min_level, sort, limit })
    }

    fn player(&self) -> &str {
        self.player
    }

    fn query_and_values(&self) -> (String, Vec<Value>) {
        let sort = self.sort.unwrap_or_default();

        let params = iter::repeat(Value::from(self.player)).take(if self.owner { 1 } else { 3 }).collect::<Vec<_>>();

        let query = format!(
            "select p.*{older_select}
from portals p
inner join portal_revisions pr on pr.id = p.latest_revision_id
{mod_reso_join}
{older_join}
where (pr.owner = ?{mod_reso_where}){min_level_where}
group by p.id order by {sort}{limit}",
            older_select =
                if sort.is_older() { ", (unix_timestamp() * 1000 - first.timestamp) / 86400000.0 days" } else { "" },
            mod_reso_join = if self.owner {
                ""
            } else {
                "left join portal_mods mods on pr.id = mods.revision_id
inner join portal_resonators reso on pr.id = reso.revision_id"
            },
            older_join = if sort.is_older() {
                "inner join (
SELECT pr1.portal_id, MAX(pr1.timestamp) timestamp
from portal_revisions pr1
left join portal_revisions pr2 on pr1.portal_id = pr2.portal_id and pr1.revision = pr2.revision + 1
where pr1.faction <> pr2.faction or pr2.id is null
group by pr1.portal_id
) first ON first.portal_id = p.id"
            } else {
                ""
            },
            mod_reso_where = if self.owner { "" } else { " or mods.mod_owner = ? or reso.reso_owner = ?" },
            min_level_where =
                if let Some(lvl) = self.min_level { format!(" and pr.level >= {lvl}") } else { String::new() },
            sort = sort.sort(),
            limit = if let Some(limit) = self.limit { format!(" limit {limit}") } else { String::new() },
        );

        (query, params)
    }
}

pub async fn execute<C>(
    conn: &C,
    client: &Client,
    message_id: i64,
    chat_id: ChatPeerId,
    filters: impl Iterator<Item = &str>,
    owner: bool,
) -> Result<(), Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let messages = match Filters::new(owner, filters) {
        Ok(filters) => {
            let (query, params) = filters.query_and_values();
            let stmt = Statement::from_sql_and_values(conn.get_database_backend(), query, params);
            conn.stream(stmt)
                .await?
                .map(|row| portal::Model::from_query_result(&row?, ""))
                .try_fold(
                    vec![format!("{}'s portals:\n", generate_player_link(filters.player()))],
                    |mut messages: Vec<String>, portal| {
                        let link = format!("{} {}", symbols::BULLET, portal.as_portal());

                        // split messages to respect 4094 bytes message limit
                        if let Some(message) = messages.last_mut() {
                            if message.len() + link.len() + 1 > 4094 {
                                messages.push(link);
                            } else {
                                message.push('\n');
                                message.push_str(&link);
                            }
                        } else {
                            messages.push(link);
                        };

                        future::ready(Ok(messages))
                    },
                )
                .await?
        }
        Err(err) => {
            vec![err]
        }
    };

    for message in messages {
        client
            .execute(
                SendMessage::new(chat_id, message)
                    .with_reply_parameters(ReplyParameters::new(message_id))
                    .with_parse_mode(ParseMode::Markdown)
                    .with_link_preview_options(LinkPreviewOptions::default().with_is_disabled(true)),
            )
            .await?;
    }

    Ok(())
}
