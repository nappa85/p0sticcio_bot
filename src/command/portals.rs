use std::{future, str::FromStr};

use chumsky::prelude::*;
use futures_util::{StreamExt, TryStreamExt};
use sea_orm::{ConnectionTrait, EntityTrait, FromQueryResult, Statement, StreamTrait, Value};
use tgbot::{
    api::Client,
    types::{ChatPeerId, LinkPreviewOptions, ParseMode, ReplyParameters, SendMessage, UserPeerId},
};

use super::Error;

use crate::{
    database::{location, portal},
    symbols,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum Sort {
    #[default]
    Name,
    Level,
    Older,
    Nearer,
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
        } else if s.eq_ignore_ascii_case("nearer") {
            Ok(Sort::Nearer)
        } else {
            Err(())
        }
    }
}

impl Sort {
    fn is_older(&self) -> bool {
        matches!(self, Sort::Older)
    }

    fn is_nearer(&self) -> bool {
        matches!(self, Sort::Nearer)
    }

    fn sort(&self) -> &'static str {
        match self {
            Sort::Name => "p.title ASC",
            Sort::Level => "pr.level DESC",
            Sort::Older => "days DESC",
            Sort::Nearer => "distance ASC",
        }
    }
}

async fn query_and_values<C>(conn: &C, user_id: i64, filters: &str) -> Result<(String, Vec<Value>), String>
where
    C: ConnectionTrait,
{
    let filters = match parser().parse_recovery(filters) {
        (_, errs) if !errs.is_empty() => {
            return Err(errs.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"));
        }
        (Some(filters), _) => filters,
        (None, _) => return Err(String::from("No output generated from given query")),
    };

    let sort = filters.iter().filter_map(Expr::sort).next().unwrap_or_default();

    let limit = filters.iter().filter_map(Expr::limit).next();

    let has_mod = filters.iter().any(|filter| filter.find_op_field(|field| field.starts_with("mod_")));
    let has_reso = filters.iter().any(|filter| filter.find_op_field(|field| field.starts_with("reso_")));

    let Some((where_clause, params)) = Expr::And(filters).get_where() else {
        return Err(String::from("No valid parameters given"));
    };

    let position = if sort.is_nearer() {
        let Ok(Some(position)) = location::Entity::find_by_id(user_id).one(conn).await else {
            return Err(String::from("Please first send your position to the bot"));
        };
        Some((position.latitude, position.longitude))
    } else {
        None
    };

    let query = format!(
        "select p.*{older_select}{position_select}
from portals p
inner join portal_revisions pr on pr.id = p.latest_revision_id
{mod_join}
{reso_join}
{older_join}
where {where_clause}
group by p.id order by {sort}{limit}",
        older_select =
            if sort.is_older() { ", (unix_timestamp() * 1000 - first.timestamp) / 86400000.0 days" } else { "" },
        position_select = if let Some((lat, lon)) = position {
            format!(", sqrt(power(abs({lat} - p.latitude), 2) + power(abs({lon} - p.longitude), 2)) distance")
        } else {
            String::new()
        },
        mod_join = if has_mod { "left join portal_mods mods on pr.id = mods.revision_id" } else { "" },
        reso_join = if has_reso { "left join portal_resonators reso on pr.id = reso.revision_id" } else { "" },
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
        sort = sort.sort(),
        limit = if let Some(limit) = limit { format!(" limit {limit}") } else { String::new() },
    );

    Ok((query, params))
}

pub async fn execute<C>(
    conn: &C,
    client: &Client,
    message_id: i64,
    chat_id: ChatPeerId,
    user_id: UserPeerId,
    filters: impl Iterator<Item = &str>,
) -> Result<(), Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let query = filters.collect::<Vec<_>>().join(" ");
    let messages = match query_and_values(conn, i64::from(user_id), &query).await {
        Ok((query, params)) => {
            let stmt = Statement::from_sql_and_values(conn.get_database_backend(), query, params);
            match conn.stream(stmt).await {
                Ok(stream) => {
                    stream
                        .map(|row| portal::Model::from_query_result(&row?, ""))
                        .try_fold(vec![], |mut messages: Vec<String>, portal| {
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
                        })
                        .await?
                }
                Err(err) => vec![err.to_string()],
            }
        }
        Err(err) => {
            vec![err.to_string()]
        }
    };

    let messages =
        if messages.is_empty() { vec![String::from("No portal found matching given filters")] } else { messages };

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

#[derive(PartialEq, Debug)]
enum Var {
    Num(u64),
    String(String),
}

impl Var {
    fn into_value(self) -> Value {
        match self {
            Self::Num(n) => Value::from(n),
            Self::String(s) => Value::from(s),
        }
    }
}

#[derive(PartialEq, Debug)]
enum Expr {
    Eq((String, Var)),
    Ne((String, Var)),
    Like((String, String)),
    Gt((String, Var)),
    Gte((String, Var)),
    Lt((String, Var)),
    Lte((String, Var)),
    Sort(Sort),
    Limit(u64),
    Or(Vec<Expr>),
    And(Vec<Expr>),
}

impl Expr {
    fn find_op_field<F>(&self, f: F) -> bool
    where
        F: for<'a> Fn(&'a str) -> bool + Copy,
    {
        match self {
            Self::Eq((s, _))
            | Self::Ne((s, _))
            | Self::Like((s, _))
            | Self::Gt((s, _))
            | Self::Gte((s, _))
            | Self::Lt((s, _))
            | Self::Lte((s, _)) => f(s.as_str()),
            Self::Or(e) | Self::And(e) => e.iter().any(|op| op.find_op_field(f)),
            _ => false,
        }
    }

    fn get_where(self) -> Option<(String, Vec<Value>)> {
        match self {
            Self::Eq((k, v)) => Some((format!("{k} = ?"), vec![v.into_value()])),
            Self::Ne((k, v)) => Some((format!("{k} <> ?"), vec![v.into_value()])),
            Self::Like((k, v)) => Some((format!("{k} like ?"), vec![Value::from(v)])),
            Self::Gt((k, v)) => Some((format!("{k} > ?"), vec![v.into_value()])),
            Self::Gte((k, v)) => Some((format!("{k} >= ?"), vec![v.into_value()])),
            Self::Lt((k, v)) => Some((format!("{k} < ?"), vec![v.into_value()])),
            Self::Lte((k, v)) => Some((format!("{k} <= ?"), vec![v.into_value()])),
            Self::Or(e) => e
                .into_iter()
                .filter_map(Expr::get_where)
                .fold(None, |acc: Option<(String, Vec<Value>)>, (r#where, params)| {
                    if let Some(mut acc) = acc {
                        acc.0.push_str(" or ");
                        acc.0.push_str(&r#where);
                        acc.1.extend(params);
                        Some(acc)
                    } else {
                        Some((r#where, params))
                    }
                })
                .map(|(s, params)| (format!("({s})"), params)),
            Self::And(e) => e
                .into_iter()
                .filter_map(Expr::get_where)
                .fold(None, |acc: Option<(String, Vec<Value>)>, (r#where, params)| {
                    if let Some(mut acc) = acc {
                        acc.0.push_str(" and ");
                        acc.0.push_str(&r#where);
                        acc.1.extend(params);
                        Some(acc)
                    } else {
                        Some((r#where, params))
                    }
                })
                .map(|(s, params)| (format!("({s})"), params)),
            _ => None,
        }
    }

    fn sort(&self) -> Option<Sort> {
        match self {
            Self::Sort(s) => Some(*s),
            _ => None,
        }
    }

    fn limit(&self) -> Option<u64> {
        match self {
            Self::Limit(i) => Some(*i),
            _ => None,
        }
    }
}

fn parser() -> impl Parser<char, Vec<Expr>, Error = Simple<char>> {
    // generic ident
    let ident = text::ident().padded().labelled("ident");

    // inspired by json
    let escape = just('\\').ignore_then(just('"'));

    // copied from json
    let string = just('"')
        .ignore_then(filter(|c| *c != '\\' && *c != '"').or(escape).repeated())
        .then_ignore(just('"'))
        .collect::<String>()
        .labelled("string");

    // number or string
    let var = text::int(10).padded().from_str().unwrapped().map(Var::Num).or(string.map(Var::String)).labelled("var");

    let expr = recursive(|expr| {
        let eq = ident.then_ignore(just('=')).then(var).map(Expr::Eq).labelled("eq");

        let ne = ident.then_ignore(just("<>")).then(var).map(Expr::Ne).labelled("ne");

        let like = ident.then_ignore(just('~')).then(string).map(Expr::Like).labelled("like");

        let gt = ident.then_ignore(just('>')).then(var).map(Expr::Gt).labelled("gt");

        let gte = ident.then_ignore(just(">=")).then(var).map(Expr::Gte).labelled("gte");

        let lt = ident.then_ignore(just('<')).then(var).map(Expr::Lt).labelled("lt");

        let lte = ident.then_ignore(just("<=")).then(var).map(Expr::Lte).labelled("lte");

        let or = just("or")
            .padded()
            .ignore_then(expr.clone().delimited_by(just('('), just(')')).collect())
            .map(Expr::Or)
            .labelled("or");

        let and = just("and")
            .padded()
            .ignore_then(expr.delimited_by(just('('), just(')')).collect())
            .map(Expr::And)
            .labelled("and");

        let sort = ident
            .validate(|field, span, emit| {
                let Ok(sort) = Sort::from_str(&field) else {
                    emit(Simple::custom(span, format!("Invalid sort {field}")));
                    return Expr::Sort(Sort::default());
                };
                Expr::Sort(sort)
            })
            .labelled("sort");

        let limit = text::int(10).padded().from_str().unwrapped().map(Expr::Limit).labelled("param");

        eq.or(ne).or(like).or(gt).or(gte).or(lt).or(lte).or(or).or(and).or(sort).or(limit).repeated()
    });

    expr.then_ignore(end())
}
