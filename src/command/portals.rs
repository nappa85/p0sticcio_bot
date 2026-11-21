use std::{borrow::Cow, future, str::FromStr};

use futures_util::{StreamExt, TryStreamExt};
use sea_orm::{ConnectionTrait, EntityTrait, FromQueryResult, Statement, StreamTrait, Value};
use winnow::{
    ModalResult, Parser,
    ascii::digit1,
    combinator::{alt, cut_err, eof, fail, preceded, repeat, terminated},
    error::{StrContext, StrContextValue},
    stream::Stream,
    token::{one_of, take_while},
};
use super::{Context, Error};

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
            Sort::Name => "lower(p.title) ASC",
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
    let filters = parse_filters(filters)?;

    let sort = filters.iter().filter_map(Expr::sort).next().unwrap_or_default();

    let limit = filters.iter().filter_map(Expr::limit).next();

    let has_mod = filters.iter().any(|filter| filter.find_op_field(|field| field.starts_with("mod_")));
    let has_reso = filters.iter().any(|filter| filter.find_op_field(|field| field.starts_with("reso_")));

    let mut index = 0;
    let Some((where_clause, params)) = Expr::And(filters).get_where(&mut index) else {
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

    // mods and resonators joins multiply rows, group them back to one row per portal;
    // grouping by both primary keys lets Postgres accept any p.* and pr.* column in select and order by
    let group_by = if has_mod || has_reso { "group by p.id, pr.id " } else { "" };

    let query = format!(
        "select p.*{older_select}{position_select}
from portals p
inner join portal_revisions pr on pr.id = p.latest_revision_id
{mod_join}
{reso_join}
where {where_clause}
{group_by}order by {sort}{limit}",
        // days since the portal switched to its current faction:
        // the revision right after the latest one with a different faction, or the first revision if it never changed
        older_select = if sort.is_older() {
            ", (extract(epoch from now()) * 1000 - (
select r.timestamp from portal_revisions r
where r.portal_id = p.id and r.revision = coalesce((
select d.revision + 1 from portal_revisions d
where d.portal_id = p.id and d.faction <> pr.faction
order by d.revision desc limit 1
), 0)
)) / 86400000.0 days"
        } else {
            ""
        },
        position_select = if let Some((lat, lon)) = position {
            format!(", sqrt(power(abs({lat} - p.latitude), 2) + power(abs({lon} - p.longitude), 2)) distance")
        } else {
            String::new()
        },
        mod_join = if has_mod { "left join portal_mods mods on pr.id = mods.revision_id" } else { "" },
        reso_join = if has_reso { "left join portal_resonators reso on pr.id = reso.revision_id" } else { "" },
        sort = sort.sort(),
        limit = if let Some(limit) = limit { format!(" limit {limit}") } else { String::new() },
    );

    Ok((query, params))
}

pub async fn execute<C>(ctx: &Context<'_, C>, filters: impl Iterator<Item = &str>) -> Result<(), Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let Context { conn, user_id, .. } = *ctx;
    let query = filters.collect::<Vec<_>>().join(" ");
    let messages = portal_messages(conn, i64::from(user_id), &query).await?;
    ctx.reply_all(messages).await
}

/// runs the filters query, returns the messages to reply with
async fn portal_messages<C>(conn: &C, user_id: i64, query: &str) -> Result<Vec<String>, Error>
where
    C: ConnectionTrait + StreamTrait,
{
    let messages = match query_and_values(conn, user_id, query).await {
        Ok((query, params)) => {
            let stmt = Statement::from_sql_and_values(ConnectionTrait::get_database_backend(conn), query.clone(), params);
            match conn.stream_raw(stmt).await {
                Ok(stream) => {
                    stream
                        .map(|row| portal::Model::from_query_result(&row?, ""))
                        .try_fold(vec![], |mut messages: Vec<String>, portal| {
                            let link = format!("{} {}", symbols::BULLET, portal.as_portal());

                            super::push_chunked(&mut messages, link);

                            future::ready(Ok(messages))
                        })
                        .await
                        .map_err(|err| Error::Query(err, query))?
                }
                Err(err) => vec![format!("Execution error: {err}, query was {query}")],
            }
        }
        Err(err) => {
            vec![format!("Parse error: {err}")]
        }
    };

    Ok(if messages.is_empty() { vec![String::from("No portal found matching given filters")] } else { messages })
}

#[derive(PartialEq, Debug)]
enum Var<'a> {
    Num(u64),
    String(Cow<'a, str>),
}

impl Var<'_> {
    fn into_value(self) -> Value {
        match self {
            Self::Num(n) => Value::from(n),
            Self::String(s) => Value::from(s.into_owned()),
        }
    }
}

/// parsed filters, borrowing from the input
#[derive(PartialEq, Debug)]
enum Expr<'a> {
    Eq((&'a str, Var<'a>)),
    Ne((&'a str, Var<'a>)),
    Like((&'a str, Cow<'a, str>)),
    Gt((&'a str, Var<'a>)),
    Gte((&'a str, Var<'a>)),
    Lt((&'a str, Var<'a>)),
    Lte((&'a str, Var<'a>)),
    Sort(Sort),
    Limit(u64),
    Or(Vec<Expr<'a>>),
    And(Vec<Expr<'a>>),
}

impl Expr<'_> {
    /// number of sql parameters `get_where` will need
    fn params(&self) -> usize {
        match self {
            Self::Eq(_) | Self::Ne(_) | Self::Like(_) | Self::Gt(_) | Self::Gte(_) | Self::Lt(_) | Self::Lte(_) => 1,
            Self::Or(e) | Self::And(e) => e.iter().map(Expr::params).sum(),
            Self::Sort(_) | Self::Limit(_) => 0,
        }
    }

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
            | Self::Lte((s, _)) => f(s),
            Self::Or(e) | Self::And(e) => e.iter().any(|op| op.find_op_field(f)),
            _ => false,
        }
    }

    fn get_where(self, index: &mut u8) -> Option<(String, Vec<Value>)> {
        match self {
            Self::Eq((k, v)) => {
                *index += 1;
                Some((format!("{k} = ${index}"), vec![v.into_value()]))
            }
            Self::Ne((k, v)) => {
                *index += 1;
                Some((format!("{k} <> ${index}"), vec![v.into_value()]))
            }
            Self::Like((k, v)) => {
                *index += 1;
                Some((format!("{k} like ${index}"), vec![Value::from(v.into_owned())]))
            }
            Self::Gt((k, v)) => {
                *index += 1;
                Some((format!("{k} > ${index}"), vec![v.into_value()]))
            }
            Self::Gte((k, v)) => {
                *index += 1;
                Some((format!("{k} >= ${index}"), vec![v.into_value()]))
            }
            Self::Lt((k, v)) => {
                *index += 1;
                Some((format!("{k} < ${index}"), vec![v.into_value()]))
            }
            Self::Lte((k, v)) => {
                *index += 1;
                Some((format!("{k} <= ${index}"), vec![v.into_value()]))
            }
            Self::Or(e) => e
                .into_iter()
                .filter_map(|expr| expr.get_where(index))
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
                .filter_map(|expr| expr.get_where(index))
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

/// parses `/portals` filters, the error is meant for the user (markdown safe)
fn parse_filters(input: &str) -> Result<Vec<Expr<'_>>, String> {
    let filters = terminated(filters, (ws, eof)).parse(input).map_err(|err| {
        let ctx = err.inner();
        let label = ctx.context().find_map(|ctx| match ctx {
            StrContext::Label(label) => Some(*label),
            _ => None,
        });
        let expected = ctx
            .context()
            .filter_map(|ctx| match ctx {
                StrContext::Expected(expected) => Some(expected.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let rest = &input[err.offset()..];
        let mut msg =
            format!("invalid {} at position {}", label.unwrap_or("filter"), input[..err.offset()].chars().count() + 1);
        if !expected.is_empty() {
            msg.push_str(", expected ");
            msg.push_str(&expected.join(" or "));
        }
        if rest.is_empty() {
            msg.push_str(": end of input");
        } else {
            // backticks would break the markdown code span
            msg.push_str(&format!(": `{}`", rest.chars().filter(|c| *c != '`').take(20).collect::<String>()));
        }
        msg
    })?;
    // sql parameters are numbered with an u8, see `Expr::get_where`
    let params = filters.iter().map(Expr::params).sum::<usize>();
    if params > usize::from(u8::MAX) {
        return Err(format!("too many conditions ({params}), at most {} are allowed", u8::MAX));
    }
    Ok(filters)
}

/// zero or more filters, each one optionally preceded by whitespace
fn filters<'a>(input: &mut &'a str) -> ModalResult<Vec<Expr<'a>>> {
    repeat(0.., preceded(ws, filter)).parse_next(input)
}

fn filter<'a>(input: &mut &'a str) -> ModalResult<Expr<'a>> {
    alt((comparison, group, sort, limit)).parse_next(input)
}

fn ws<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    take_while(0.., char::is_whitespace).parse_next(input)
}

/// ascii identifier, it's written verbatim into the sql
fn ident<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    (
        one_of(|c: char| c.is_ascii_alphabetic() || c == '_'),
        take_while(0.., |c: char| c.is_ascii_alphanumeric() || c == '_'),
    )
        .take()
        .parse_next(input)
}

/// `{field}{comparison}{value}`, committed once the comparison operator is found
fn comparison<'a>(input: &mut &'a str) -> ModalResult<Expr<'a>> {
    let field = ident.parse_next(input)?;
    let op = preceded(ws, alt(("<>", "<=", ">=", "<", ">", "=", "~"))).parse_next(input)?;
    ws.parse_next(input)?;
    // contexts on the last branch, they're for values that don't even start
    const NUMBER: StrContext = StrContext::Expected(StrContextValue::Description("number"));
    const STRING: StrContext = StrContext::Expected(StrContextValue::Description("string"));
    if op == "~" {
        let value = cut_err(alt((string, fail.context(STRING)))).parse_next(input)?;
        return Ok(Expr::Like((field, value)));
    }
    let value = cut_err(alt((number.map(Var::Num), string.map(Var::String), fail.context(NUMBER).context(STRING))))
        .parse_next(input)?;
    Ok(match op {
        "<>" => Expr::Ne((field, value)),
        "<=" => Expr::Lte((field, value)),
        ">=" => Expr::Gte((field, value)),
        "<" => Expr::Lt((field, value)),
        ">" => Expr::Gt((field, value)),
        _ => Expr::Eq((field, value)),
    })
}

/// `or([filters])` or `and([filters])`, committed once the open parenthesis is found
fn group<'a>(input: &mut &'a str) -> ModalResult<Expr<'a>> {
    let kind = terminated(alt(("or", "and")), (ws, '(')).parse_next(input)?;
    let filters =
        terminated(filters, (ws, cut_err(')').context(StrContext::Expected(StrContextValue::CharLiteral(')')))))
            .parse_next(input)?;
    Ok(if kind == "or" { Expr::Or(filters) } else { Expr::And(filters) })
}

/// a lone identifier is a sort, an unknown one is an error
fn sort<'a>(input: &mut &'a str) -> ModalResult<Expr<'a>> {
    let checkpoint = input.checkpoint();
    let word = ident.parse_next(input)?;
    match Sort::from_str(word) {
        Ok(sort) => Ok(Expr::Sort(sort)),
        Err(()) => {
            input.reset(&checkpoint);
            cut_err(fail)
                .context(StrContext::Label("sort"))
                .context(StrContext::Expected(StrContextValue::Description("name, level, older or nearer")))
                .parse_next(input)
        }
    }
}

fn limit<'a>(input: &mut &'a str) -> ModalResult<Expr<'a>> {
    number.map(Expr::Limit).parse_next(input)
}

/// decimal u64, too big numbers are an error
fn number(input: &mut &str) -> ModalResult<u64> {
    let checkpoint = input.checkpoint();
    let digits = digit1.parse_next(input)?;
    match digits.parse() {
        Ok(n) => Ok(n),
        Err(_) => {
            input.reset(&checkpoint);
            cut_err(fail).context(StrContext::Label("number")).parse_next(input)
        }
    }
}

/// quoted string, `\"` is the only escape, borrowed unless it contains escapes;
/// phones autocorrect quotes, so typographic ones are accepted too, each closed by its counterparts
fn string<'a>(input: &mut &'a str) -> ModalResult<Cow<'a, str>> {
    let open = one_of(['"', '“', '”', '„', '«', '»']).parse_next(input)?;
    let close: &[char] = match open {
        '"' => &['"'],
        '«' => &['»'],
        '»' => &['«'],
        // “English”, ”Swedish”, „German“, „Polish”
        _ => &['”', '“'],
    };
    let raw = repeat::<_, _, (), _, _>(
        0..,
        alt((take_while(1.., |c: char| c != '\\' && !close.contains(&c)).void(), "\\\"".void())),
    )
    .take()
    .parse_next(input)?;
    cut_err(one_of(close)).context(StrContext::Expected(StrContextValue::CharLiteral(close[0]))).parse_next(input)?;
    Ok(if raw.contains('\\') { Cow::Owned(raw.replace("\\\"", "\"")) } else { Cow::Borrowed(raw) })
}

#[cfg(test)]
mod tests {
    use sea_orm::Value;

    use super::{Expr, Sort, Var, parse_filters};

    fn num(n: u64) -> Var<'static> {
        Var::Num(n)
    }
    fn s(s: &str) -> Var<'_> {
        Var::String(s.into())
    }
    fn eq<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Eq((f, v))
    }
    fn ne<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Ne((f, v))
    }
    fn gt<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Gt((f, v))
    }
    fn gte<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Gte((f, v))
    }
    fn lt<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Lt((f, v))
    }
    fn lte<'a>(f: &'a str, v: Var<'a>) -> Expr<'a> {
        Expr::Lte((f, v))
    }
    fn like<'a>(f: &'a str, v: &'a str) -> Expr<'a> {
        Expr::Like((f, v.into()))
    }

    #[track_caller]
    fn ok(input: &str, expected: Vec<Expr<'_>>) {
        assert_eq!(parse_filters(input), Ok(expected), "{input:?}");
    }

    #[track_caller]
    fn err(input: &str) {
        let res = parse_filters(input);
        assert!(res.is_err(), "{input:?} => {res:?}");
    }

    #[test]
    fn comparisons() {
        ok("title=\"x\"", vec![eq("title", s("x"))]);
        ok("title = \"x\"", vec![eq("title", s("x"))]);
        ok("level =5", vec![eq("level", num(5))]);
        ok("level<>5", vec![ne("level", num(5))]);
        ok("level>5", vec![gt("level", num(5))]);
        ok("level>=5", vec![gte("level", num(5))]);
        ok("level<5", vec![lt("level", num(5))]);
        ok("level<=5", vec![lte("level", num(5))]);
        ok("level <= 5", vec![lte("level", num(5))]);
        ok("title~\"%a%\"", vec![like("title", "%a%")]);
        ok("mod_name = \"Ito En Transmuter (-)\"", vec![eq("mod_name", s("Ito En Transmuter (-)"))]);
        ok("_x=1", vec![eq("_x", num(1))]);
        ok("x1=1", vec![eq("x1", num(1))]);
        ok("a=\"\"", vec![eq("a", s(""))]);
        ok(" title=\"x\"", vec![eq("title", s("x"))]);
    }

    #[test]
    fn strings() {
        ok(r#"title="a \"b\"""#, vec![eq("title", s("a \"b\""))]);
        ok("title=\"Città\"", vec![eq("title", s("Città"))]);
        ok("title=\"a\nb\"", vec![eq("title", s("a\nb"))]);
        // only \" is a valid escape
        err(r#"title="a\b""#);
        err("title=\"unterminated");
    }

    #[test]
    fn groups() {
        ok("or(a=1 b=2)", vec![Expr::Or(vec![eq("a", num(1)), eq("b", num(2))])]);
        ok("or ( a=1 b=2 )", vec![Expr::Or(vec![eq("a", num(1)), eq("b", num(2))])]);
        ok("or(title=\"x\")", vec![Expr::Or(vec![eq("title", s("x"))])]);
        ok(
            "and(a=1 or(b=2 c=3))",
            vec![Expr::And(vec![eq("a", num(1)), Expr::Or(vec![eq("b", num(2)), eq("c", num(3))])])],
        );
        ok("or()", vec![Expr::Or(vec![])]);
        err("or(a=1");
        err("a=1)");
        err("orx(a=1)");
    }

    #[test]
    fn sorts_and_limits() {
        ok("", vec![]);
        ok("name", vec![Expr::Sort(Sort::Name)]);
        ok("Nearer", vec![Expr::Sort(Sort::Nearer)]);
        ok("LEVEL", vec![Expr::Sort(Sort::Level)]);
        ok("10", vec![Expr::Limit(10)]);
        ok("older 10", vec![Expr::Sort(Sort::Older), Expr::Limit(10)]);
        ok("owner=\"Niantic\" older 10", vec![eq("owner", s("Niantic")), Expr::Sort(Sort::Older), Expr::Limit(10)]);
        ok("a = 1 10 20", vec![eq("a", num(1)), Expr::Limit(10), Expr::Limit(20)]);
        err("foo");
        err("ord");
    }

    #[test]
    fn invalid() {
        err("title~5");
        err("level=-1");
        err("a=1,b=2");
        err("a==1");
        // identifiers are ascii only, they end up verbatim in the sql
        err("città=1");
        err("a;drop=1");
        err("a.b=1");
        err("a=1 --");
    }

    /// odd but accepted inputs, kept as they are
    #[test]
    fn lenient() {
        ok("a=1and(b=2)", vec![eq("a", num(1)), Expr::And(vec![eq("b", num(2))])]);
        ok("a=1 b=\"x\"c=2", vec![eq("a", num(1)), eq("b", s("x")), eq("c", num(2))]);
        ok("1x=1", vec![Expr::Limit(1), eq("x", num(1))]);
    }

    /// chumsky 0.9 quirks fixed by the winnow parser
    #[test]
    fn quirks() {
        // was level=0 limit 5, integers couldn't have leading zeros
        ok("level=05", vec![eq("level", num(5))]);
        // were errors, strings weren't padded
        ok("title=\"x\" ", vec![eq("title", s("x"))]);
        ok("or(title=\"x\" )", vec![Expr::Or(vec![eq("title", s("x"))])]);
        // was a panic, chumsky used `unwrapped()`
        ok("18446744073709551615", vec![Expr::Limit(u64::MAX)]);
        err("18446744073709551616");
        err("level=99999999999999999999");
    }

    #[test]
    fn typographic_quotes() {
        ok("owner=“Niantic”", vec![eq("owner", s("Niantic"))]);
        ok("owner=”Niantic”", vec![eq("owner", s("Niantic"))]);
        ok("title=„Fontana“", vec![eq("title", s("Fontana"))]);
        ok("title=„Fontana”", vec![eq("title", s("Fontana"))]);
        ok("title=«Fontana»", vec![eq("title", s("Fontana"))]);
        ok("title=»Fontana«", vec![eq("title", s("Fontana"))]);
        ok("title~“%Font%” level", vec![like("title", "%Font%"), Expr::Sort(Sort::Level)]);
        ok("or(owner=“a” owner=«b»)", vec![Expr::Or(vec![eq("owner", s("a")), eq("owner", s("b"))])]);
        // other kinds of quotes are just content
        ok("title=“La «Fontana» \"vecchia\"”", vec![eq("title", s("La «Fontana» \"vecchia\""))]);
        ok("title=\"say “hi”\"", vec![eq("title", s("say “hi”"))]);
        ok("title=«it's “ok”»", vec![eq("title", s("it's “ok”"))]);
        // ascii quotes are strict
        err("title=\"Fontana”");
        err("title=“Fontana\"");
        err("title=«Fontana”");
        err("title=‘Fontana’");
    }

    #[test]
    fn parameters_limit() {
        let filters = |n: usize| (0..n).map(|i| format!("a{i}=1")).collect::<Vec<_>>().join(" ");
        let input = filters(255);
        let (clause, params) = where_of(&input).unwrap();
        assert!(clause.ends_with("a254 = $255)"), "{clause}");
        assert_eq!(params.len(), 255);
        // sorts and limits don't count
        assert!(parse_filters(&format!("{input} older 10")).is_ok());
        assert_eq!(
            parse_filters(&filters(256)),
            Err(String::from("too many conditions (256), at most 255 are allowed"))
        );
        // nested ones do
        let nested = format!("a=1 or({} and(b=2))", filters(254));
        assert_eq!(parse_filters(&nested), Err(String::from("too many conditions (256), at most 255 are allowed")));
    }

    #[test]
    fn zero_copy() {
        let input = String::from("title=\"plain\" owner=\"with \\\"escape\\\"\" mod_name=“typographic”");
        let filters = parse_filters(&input).unwrap();
        let [
            Expr::Eq((title, Var::String(plain))),
            Expr::Eq((_, Var::String(escaped))),
            Expr::Eq((_, Var::String(typographic))),
        ] = filters.as_slice()
        else {
            panic!("{filters:?}")
        };
        assert!(input.as_bytes().as_ptr_range().contains(&title.as_ptr()));
        assert!(matches!(plain, std::borrow::Cow::Borrowed(_)));
        assert!(matches!(typographic, std::borrow::Cow::Borrowed(_)));
        assert!(matches!(escaped, std::borrow::Cow::Owned(_)));
    }

    #[test]
    fn error_messages() {
        #[track_caller]
        fn msg(input: &str, expected: &str) {
            assert_eq!(parse_filters(input).unwrap_err(), expected, "{input:?}");
        }
        msg("foo", "invalid sort at position 1, expected name, level, older or nearer: `foo`");
        msg("level>5 mod_name", "invalid sort at position 9, expected name, level, older or nearer: `mod_name`");
        msg("title~5", "invalid filter at position 7, expected string: `5`");
        msg("a==1", "invalid filter at position 3, expected number or string: `=1`");
        msg("title=\"unterminated", "invalid filter at position 20, expected `\"`: end of input");
        msg(r#"title="a\b""#, "invalid filter at position 9, expected `\"`: `\\b\"`");
        msg("or(a=1", "invalid filter at position 7, expected `)`: end of input");
        msg("a=1,b=2", "invalid filter at position 4: `,b=2`");
        msg("città=1", "invalid sort at position 1, expected name, level, older or nearer: `città=1`");
        msg("level=99999999999999999999", "invalid number at position 7: `99999999999999999999`");
        msg("title=“unterminated\"", "invalid filter at position 21, expected `”`: end of input");
        msg("a=`x`", "invalid filter at position 3, expected number or string: `x`");
    }

    fn where_of(input: &str) -> Option<(String, Vec<Value>)> {
        Expr::And(parse_filters(input).unwrap()).get_where(&mut 0)
    }

    #[test]
    fn where_clause() {
        assert_eq!(where_of(""), None);
        assert_eq!(where_of("older 10"), None);
        assert_eq!(
            where_of("owner=\"Niantic\" level>=7 older 10"),
            Some((String::from("(owner = $1 and level >= $2)"), vec![Value::from("Niantic"), Value::from(7u64)]))
        );
        assert_eq!(
            where_of("faction=\"E\" or(title~\"%a%\" and(level<3 res_count<>8)) name"),
            Some((
                String::from("(faction = $1 and (title like $2 or (level < $3 and res_count <> $4)))"),
                vec![Value::from("E"), Value::from("%a%"), Value::from(3u64), Value::from(8u64)]
            ))
        );
        assert_eq!(where_of("or()"), None);
    }

    mod database {
        use sea_orm::{ActiveModelTrait, ActiveValue, DatabaseConnection};

        use super::super::*;
        use crate::database::testing::{Spec, db, scan};

        // 2025-01-01, 2026-01-01, 2026-03-01, 2026-06-01
        const T2025: i64 = 1_735_689_600_000;
        const T2026_01: i64 = 1_767_225_600_000;
        const T2026_03: i64 = 1_772_323_200_000;
        const T2026_06: i64 = 1_780_272_000_000;

        async fn seed(conn: &DatabaseConnection) {
            let fontana = Spec { title: "Fontana", lat: 45.1, lon: 11.1, ..Default::default() };
            scan(conn, "aa.1", T2025, Spec { faction: "R", owner: Some("carol"), level: 2, ..fontana }).await;
            scan(
                conn,
                "aa.1",
                T2026_06,
                Spec {
                    faction: "E",
                    level: 7,
                    owner: Some("alice"),
                    resonators: &[("alice", 8, 6000), ("bob", 7, 5000)],
                    mods: &[("bob", "Heat Sink", "VERY_RARE"), ("alice", "Portal Shield", "COMMON")],
                    ..fontana
                },
            )
            .await;
            scan(
                conn,
                "bb.1",
                T2026_01,
                Spec {
                    title: "Chiesa",
                    faction: "R",
                    lat: 45.2,
                    lon: 11.2,
                    level: 3,
                    owner: Some("carol"),
                    resonators: &[("carol", 3, 2000)],
                    ..Default::default()
                },
            )
            .await;
            scan(conn, "cc.1", T2026_03, Spec { title: "Duomo", lat: 46.0, lon: 12.0, ..Default::default() }).await;
        }

        /// portal titles in the reply, or the reply itself when there are none
        async fn titles(conn: &DatabaseConnection, query: &str) -> Result<Vec<String>, String> {
            let messages = portal_messages(conn, 7, query).await.map_err(|err| format!("Err: {err}"))?;
            let lines = messages.iter().flat_map(|message| message.lines()).collect::<Vec<_>>();
            if lines.iter().all(|line| line.starts_with(symbols::BULLET)) {
                Ok(lines
                    .iter()
                    .map(|line| line.split_once('[').unwrap().1.split_once(']').unwrap().0.to_owned())
                    .collect())
            } else {
                Err(messages.join("\n"))
            }
        }

        #[track_caller]
        fn assert_titles(res: Result<Vec<String>, String>, expected: &[&str]) {
            assert_eq!(res, Ok(expected.iter().map(|t| t.to_string()).collect::<Vec<_>>()));
        }

        #[tokio::test]
        async fn filters() {
            let Some(db) = db().await else { return };
            let conn = &db.conn;
            seed(conn).await;

            assert_titles(titles(conn, "level>=1").await, &["Chiesa", "Duomo", "Fontana"]);
            assert_titles(titles(conn, "owner=\"alice\"").await, &["Fontana"]);
            assert_titles(titles(conn, "owner=“alice”").await, &["Fontana"]);
            assert_titles(titles(conn, "title~«%o%»").await, &["Duomo", "Fontana"]);
            assert_titles(titles(conn, "faction=\"N\"").await, &["Duomo"]);
            assert_titles(titles(conn, "level<>3 level<7").await, &["Duomo"]);
            assert_titles(titles(conn, "title~\"%o%\"").await, &["Duomo", "Fontana"]);
            assert_titles(titles(conn, "or(owner=\"alice\" owner=\"carol\")").await, &["Chiesa", "Fontana"]);
            assert_titles(titles(conn, "or(owner=\"alice\" owner=\"carol\") 1").await, &["Chiesa"]);
            assert_titles(titles(conn, "or(faction=\"N\" and(level>2 level<=3))").await, &["Chiesa", "Duomo"]);
            // joins, one row per portal anyway
            assert_titles(titles(conn, "mod_name=\"Heat Sink\"").await, &["Fontana"]);
            assert_titles(titles(conn, "reso_level>=1").await, &["Chiesa", "Fontana"]);
            assert_titles(titles(conn, "reso_owner=\"bob\" mod_rarity=\"VERY_RARE\" level").await, &["Fontana"]);
            assert_titles(titles(conn, "reso_energy>1000 mod_owner~\"%\" older").await, &["Fontana"]);
        }

        #[tokio::test]
        async fn sorts() {
            let Some(db) = db().await else { return };
            let conn = &db.conn;
            seed(conn).await;

            assert_titles(titles(conn, "level>=0 name").await, &["Chiesa", "Duomo", "Fontana"]);
            assert_titles(titles(conn, "level>=0 level").await, &["Fontana", "Chiesa", "Duomo"]);
            assert_titles(titles(conn, "level>=0 level 2").await, &["Fontana", "Chiesa"]);
            // Fontana changed faction in 2026-06
            assert_titles(titles(conn, "level>=0 older").await, &["Chiesa", "Duomo", "Fontana"]);

            assert_eq!(
                titles(conn, "level>=0 nearer").await,
                Err(String::from("Parse error: Please first send your position to the bot"))
            );
            location::ActiveModel {
                user_id: ActiveValue::Set(7),
                latitude: ActiveValue::Set(46.1),
                longitude: ActiveValue::Set(12.1),
            }
            .insert(conn)
            .await
            .unwrap();
            assert_titles(titles(conn, "level>=0 nearer").await, &["Duomo", "Chiesa", "Fontana"]);
        }

        #[tokio::test]
        async fn errors() {
            let Some(db) = db().await else { return };
            let conn = &db.conn;
            seed(conn).await;

            assert_eq!(titles(conn, "").await, Err(String::from("Parse error: No valid parameters given")));
            assert_eq!(titles(conn, "older 10").await, Err(String::from("Parse error: No valid parameters given")));
            let res = titles(conn, "level=foo").await;
            assert!(res.as_ref().is_err_and(|err| err.starts_with("Parse error: ")), "{res:?}");
            assert_eq!(
                titles(conn, "owner=\"nobody\"").await,
                Err(String::from("No portal found matching given filters"))
            );
            let res = titles(conn, "nonexistent=1").await;
            assert!(
                res.as_ref().is_err_and(|err| err.starts_with("Err: Database error: ")
                    && err.contains("column \"nonexistent\" does not exist")
                    && err.contains("where (nonexistent = $1)")),
                "{res:?}"
            );
        }
    }
}
