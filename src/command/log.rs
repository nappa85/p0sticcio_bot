use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement, Value,
};
use tgbot::{
    api::Client,
    types::{ChatPeerId, LinkPreviewOptions, ParseMode, ReplyParameters, SendMessage},
};

use super::Error;

use crate::{
    database::{
        portal, portal_mods, portal_resonators,
        portal_revision::{self, Faction},
    },
    entities::generate_player_link,
    symbols,
};

/// maximum number of matching portals listed to choose from
const MAX_CHOICES: usize = 10;
/// maximum number of changes shown per portal
const MAX_CHANGES: usize = 10;
/// revisions loaded per query, most of them are only energy changes we skip
const BATCH_SIZE: u64 = 500;
/// revisions scanned per portal before giving up and offering to continue from there
const MAX_SCANNED: usize = 20_000;
/// cursor format, millisecond precision makes paging exact
const CURSOR_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";

type ResoKey = (String, i64);
type ModKey = (String, String, String);

pub async fn execute<C>(
    conn: &C,
    client: &Client,
    message_id: i64,
    chat_id: ChatPeerId,
    name: impl Iterator<Item = &str>,
) -> Result<(), Error>
where
    C: ConnectionTrait,
{
    let mut words = name.collect::<Vec<_>>();
    // optional trailing date, shows only changes before it
    let before = words.last().and_then(|word| parse_before(word));
    if before.is_some() {
        words.pop();
    }
    // backticks would break the markdown when echoing the name back
    let name = words.join(" ").replace('`', "");
    let date = before.and_then(DateTime::from_timestamp_millis).map(|time| format!(" {}", time.format(CURSOR_FORMAT)));

    let messages = if name.is_empty() {
        vec![String::from("Usage: /log `portal name or id` `[YYYY-MM-DD]`")]
    } else {
        let portals = find_portals(conn, &name).await?;
        match portals.as_slice() {
            [] => vec![format!("No portal found matching `{name}`")],
            [portal] => {
                let mut messages = vec![];
                for block in portal_log(conn, portal, before).await? {
                    super::push_chunked(&mut messages, block);
                }
                messages
            }
            portals => {
                // ask which one, the portal id is unambiguous
                let mut messages = vec![String::from("More portals match, which one?")];
                for portal in portals.iter().take(MAX_CHOICES) {
                    super::push_chunked(
                        &mut messages,
                        format!(
                            "{} {}\n`/log {}{}`",
                            symbols::BULLET,
                            portal.as_portal(),
                            portal.portal_id,
                            date.as_deref().unwrap_or_default()
                        ),
                    );
                }
                if portals.len() > MAX_CHOICES {
                    super::push_chunked(&mut messages, String::from("… and more, please be more specific"));
                }
                messages
            }
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

/// portal id first, then exact case insensitive name match, substring name match as fallback
async fn find_portals<C: ConnectionTrait>(conn: &C, name: &str) -> Result<Vec<portal::Model>, DbErr> {
    let limit = MAX_CHOICES + 1;
    for (condition, param) in [
        ("portal_id = $1", Value::from(name)),
        ("lower(title) = lower($1)", Value::from(name)),
        ("title ilike $1", Value::from(format!("%{name}%"))),
    ] {
        let portals = portal::Entity::find()
            .from_raw_sql(Statement::from_sql_and_values(
                conn.get_database_backend(),
                format!("select * from portals where {condition} order by title, id limit {limit}"),
                [param],
            ))
            .all(conn)
            .await?;
        if !portals.is_empty() {
            return Ok(portals);
        }
    }
    Ok(vec![])
}

/// `2026-07-28` means up to the end of that day (UTC), a full timestamp is an exact exclusive cursor
fn parse_before(word: &str) -> Option<i64> {
    if let Ok(time) = NaiveDateTime::parse_from_str(word, CURSOR_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(word, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(word, "%Y-%m-%dT%H:%M"))
    {
        return Some(time.and_utc().timestamp_millis());
    }
    let date = NaiveDate::parse_from_str(word, "%Y-%m-%d").ok()?;
    Some(date.checked_add_days(Days::new(1))?.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

async fn portal_log<C: ConnectionTrait>(
    conn: &C,
    portal: &portal::Model,
    before: Option<i64>,
) -> Result<Vec<String>, DbErr> {
    let mut blocks = vec![format!("{} {}", symbols::BULLET, portal.as_portal())];

    let mut changes = 0;
    let mut scanned = 0;
    let mut exhausted = false;
    // revisions come newest first, this is the one waiting to be compared with its predecessor
    let mut newer: Option<State> = None;
    'outer: loop {
        let cursor = newer.as_ref().map(|state| state.revision.revision);
        let batch = load_batch(conn, portal.id, before, cursor).await?;
        let last_batch = (batch.len() as u64) < BATCH_SIZE;
        scanned += batch.len();

        for older in batch {
            if let Some(newer) = &newer
                && let Some(block) = describe(newer, Some(&older))
            {
                blocks.push(block);
                changes += 1;
            }
            newer = Some(older);
            if changes == MAX_CHANGES {
                break 'outer;
            }
        }

        if last_batch {
            exhausted = true;
            break;
        }
        if scanned >= MAX_SCANNED {
            break;
        }
    }

    if exhausted {
        // the whole history has been loaded, show the oldest revision as starting point
        if let Some(first) = &newer
            && let Some(block) = describe(first, None)
        {
            blocks.push(block);
            changes += 1;
        }
    } else if let Some(newer) = &newer
        && let Some(cursor) = DateTime::from_timestamp_millis(newer.revision.timestamp + 1)
    {
        // continue right from the revision still to be compared
        blocks.push(format!(
            "Older changes: `/log {} {}`",
            portal.portal_id,
            cursor.format(CURSOR_FORMAT)
        ));
    }

    if changes == 0 {
        blocks.insert(1, String::from("No recorded changes in the scanned period"));
    }

    Ok(blocks)
}

async fn load_batch<C: ConnectionTrait>(
    conn: &C,
    portal_id: i64,
    before: Option<i64>,
    cursor: Option<i64>,
) -> Result<Vec<State>, DbErr> {
    let mut query = portal_revision::Entity::find().filter(portal_revision::Column::PortalId.eq(portal_id));
    if let Some(before) = before {
        query = query.filter(portal_revision::Column::Timestamp.lt(before));
    }
    if let Some(cursor) = cursor {
        query = query.filter(portal_revision::Column::Revision.lt(cursor));
    }
    let revisions = query.order_by_desc(portal_revision::Column::Revision).limit(BATCH_SIZE).all(conn).await?;
    let ids = revisions.iter().map(|revision| revision.id).collect::<Vec<_>>();

    let mut resonators: HashMap<i64, Vec<ResoKey>> = HashMap::new();
    for reso in portal_resonators::Entity::find()
        .filter(portal_resonators::Column::RevisionId.is_in(ids.clone()))
        .all(conn)
        .await?
    {
        resonators.entry(reso.revision_id).or_default().push((reso.reso_owner, reso.reso_level));
    }

    let mut mods: HashMap<i64, Vec<ModKey>> = HashMap::new();
    for r#mod in portal_mods::Entity::find().filter(portal_mods::Column::RevisionId.is_in(ids)).all(conn).await? {
        mods.entry(r#mod.revision_id).or_default().push((r#mod.mod_owner, r#mod.mod_name, r#mod.mod_rarity));
    }

    Ok(revisions
        .into_iter()
        .map(|revision| State {
            resonators: resonators.remove(&revision.id).unwrap_or_default(),
            mods: mods.remove(&revision.id).unwrap_or_default(),
            revision,
        })
        .collect())
}

struct State {
    revision: portal_revision::Model,
    resonators: Vec<ResoKey>,
    mods: Vec<ModKey>,
}

/// describes what changed from `old` to `new`, None if only energy changed
fn describe(new: &State, old: Option<&State>) -> Option<String> {
    let rev = &new.revision;
    let team = team(rev.faction);

    let (event, faction_changed) = match old {
        None => (Some(String::from("first recorded")), false),
        Some(old) if old.revision.faction != rev.faction => {
            if rev.faction == Faction::Neutral {
                (Some(String::from("neutralized")), true)
            } else {
                (Some(format!("captured by {}", owner(rev.owner.as_deref()))), true)
            }
        }
        Some(old) if old.revision.owner != rev.owner => (
            Some(format!("owner {} → {}", owner(old.revision.owner.as_deref()), owner(rev.owner.as_deref()))),
            false,
        ),
        Some(_) => (None, false),
    };

    let empty = State { revision: rev.clone(), resonators: vec![], mods: vec![] };
    let old_state = old.unwrap_or(&empty);
    let old_team = team_of(old_state);

    let mut lines = vec![];
    for ((agent, level), count) in diff(&old_state.resonators, &new.resonators) {
        // on faction change everything of the previous faction is obviously gone, don't list it
        if count < 0 && faction_changed {
            continue;
        }
        let (sign, team) = if count > 0 { ('+', team) } else { ('-', old_team) };
        lines.push(format!(
            " ► {sign}{}L{level} resonator by {team} {}",
            times(count),
            generate_player_link(agent)
        ));
    }
    for ((agent, name, rarity), count) in diff(&old_state.mods, &new.mods) {
        if count < 0 && faction_changed {
            continue;
        }
        let (sign, team) = if count > 0 { ('+', team) } else { ('-', old_team) };
        lines.push(format!(
            " ► {sign}{}{} {name} by {team} {}",
            times(count),
            pretty_rarity(rarity),
            generate_player_link(agent)
        ));
    }

    if event.is_none() && lines.is_empty() {
        return None;
    }

    let time = DateTime::from_timestamp_millis(rev.timestamp)
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let mut block = format!("`{time}` {team} L{}", rev.level);
    if let Some(event) = event {
        block.push(' ');
        block.push_str(&event);
    }
    for line in lines {
        block.push('\n');
        block.push_str(&line);
    }
    Some(block)
}

/// multiset difference, positive counts are additions, negative counts are removals
fn diff<'a, T: Ord>(old: &'a [T], new: &'a [T]) -> BTreeMap<&'a T, i64> {
    let mut counts = BTreeMap::new();
    for item in new {
        *counts.entry(item).or_insert(0) += 1;
    }
    for item in old {
        *counts.entry(item).or_insert(0) -= 1;
    }
    counts.retain(|_, count| *count != 0);
    counts
}

fn times(count: i64) -> String {
    if count.abs() > 1 { format!("{}× ", count.abs()) } else { String::new() }
}

fn team(faction: Faction) -> &'static str {
    match faction {
        Faction::Neutral => symbols::WHITE,
        Faction::Enlightened => symbols::GREEN,
        Faction::Resistance => symbols::BLUE,
        Faction::Machina => symbols::RED,
    }
}

fn team_of(state: &State) -> &'static str {
    team(state.revision.faction)
}

fn owner(owner: Option<&str>) -> String {
    owner.map(generate_player_link).unwrap_or_else(|| String::from("unknown"))
}

/// VERY_RARE -> Very Rare, underscores would also break the markdown
fn pretty_rarity(rarity: &str) -> String {
    rarity
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(faction: Faction, owner: Option<&str>, resonators: &[(&str, i64)], mods: &[(&str, &str, &str)]) -> State {
        State {
            revision: portal_revision::Model {
                id: 0,
                portal_id: 0,
                revision: 0,
                timestamp: 0,
                faction,
                level: 1,
                health: 100,
                res_count: resonators.len() as i64,
                owner: owner.map(ToOwned::to_owned),
            },
            resonators: resonators.iter().map(|(o, l)| (o.to_string(), *l)).collect(),
            mods: mods.iter().map(|(o, n, r)| (o.to_string(), n.to_string(), r.to_string())).collect(),
        }
    }

    #[test]
    fn before_parsing() {
        // a day means up to its end
        assert_eq!(parse_before("2026-07-28"), parse_before("2026-07-29T00:00"));
        // the cursor written in the footer is parsed back exactly
        let cursor = DateTime::from_timestamp_millis(1_785_000_000_123).unwrap().format(CURSOR_FORMAT).to_string();
        assert_eq!(parse_before(&cursor), Some(1_785_000_000_123));
        assert_eq!(parse_before("Fontana"), None);
    }

    #[test]
    fn energy_only_change_is_skipped() {
        let old = state(Faction::Enlightened, Some("a"), &[("a", 8), ("b", 7)], &[]);
        let mut new = state(Faction::Enlightened, Some("a"), &[("b", 7), ("a", 8)], &[]);
        new.revision.health = 40;
        assert_eq!(describe(&new, Some(&old)), None);
    }

    #[test]
    fn resonator_and_mod_changes() {
        let old = state(Faction::Enlightened, Some("a"), &[("a", 8), ("b", 5)], &[]);
        let new =
            state(Faction::Enlightened, Some("a"), &[("a", 8), ("b", 8), ("b", 8)], &[("b", "Heat Sink", "VERY_RARE")]);
        let block = describe(&new, Some(&old)).unwrap();
        assert!(block.contains("+2× L8 resonator by"), "{block}");
        assert!(block.contains("-L5 resonator by"), "{block}");
        assert!(block.contains("+Very Rare Heat Sink by"), "{block}");
    }

    #[test]
    fn capture_hides_removals() {
        let old = state(Faction::Neutral, None, &[], &[]);
        let new = state(Faction::Resistance, Some("c"), &[("c", 8)], &[]);
        let block = describe(&new, Some(&old)).unwrap();
        assert!(block.contains("captured by"), "{block}");

        let old = state(Faction::Enlightened, Some("a"), &[("a", 8)], &[]);
        let new = state(Faction::Neutral, None, &[], &[]);
        let block = describe(&new, Some(&old)).unwrap();
        assert!(block.contains("neutralized") && !block.contains('►'), "{block}");
    }
}
