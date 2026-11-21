//! portals hit by a virus, polled after the virus cooldown until a second virus or a maximum time

use sea_orm::{ActiveValue, Condition, entity::prelude::*};

use super::portal_revision::Faction;

/// nobody can use a virus on the portal again for this long, milliseconds
pub const COOLDOWN: i64 = 3_600_000;
/// how long a portal is polled after the cooldown, milliseconds
pub const DURATION: i64 = 24 * 3_600_000;
/// the same virus is spotted again by later comm polls, possibly with a slightly different time
const SAME_VIRUS: i64 = 10 * 60_000;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_watches")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub virus_time: i64,
    pub faction: Option<Faction>,
    pub ended_at: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// what a spotted virus meant for the watch of its portal
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Virus {
    /// first virus, the portal is going to be polled after the cooldown
    Started,
    /// a virus already known
    Known,
    /// second virus, the watch is over
    Second,
}

/// records a virus spotted in the comm at `time` on the portal at the coordinates
pub async fn on_virus<C: ConnectionTrait>(conn: &C, lat: f64, lon: f64, time: i64) -> Result<Virus, DbErr> {
    let watch = Entity::find().filter(Column::Latitude.eq(lat)).filter(Column::Longitude.eq(lon)).one(conn).await?;
    let Some(watch) = watch else {
        ActiveModel {
            latitude: ActiveValue::Set(lat),
            longitude: ActiveValue::Set(lon),
            virus_time: ActiveValue::Set(time),
            faction: ActiveValue::Set(None),
            ended_at: ActiveValue::Set(None),
            ..Default::default()
        }
        .insert(conn)
        .await?;
        return Ok(Virus::Started);
    };

    if time < watch.virus_time + SAME_VIRUS {
        return Ok(Virus::Known);
    }
    match watch.ended_at {
        // the virus that ended it, possibly spotted after a poll saw the faction change
        Some(ended_at) if time <= ended_at + SAME_VIRUS => return Ok(Virus::Known),
        // still watching
        None if time < watch.virus_time + COOLDOWN + DURATION => {
            end(conn, watch.id, time).await?;
            return Ok(Virus::Second);
        }
        // ended or expired, this is a new first virus
        _ => {}
    }
    ActiveModel {
        id: ActiveValue::Unchanged(watch.id),
        virus_time: ActiveValue::Set(time),
        faction: ActiveValue::Set(None),
        ended_at: ActiveValue::Set(None),
        ..Default::default()
    }
    .update(conn)
    .await?;
    Ok(Virus::Started)
}

/// watches to poll at `now`: past the cooldown, not ended, not expired
pub async fn active<C: ConnectionTrait>(conn: &C, now: i64) -> Result<Vec<Model>, DbErr> {
    Entity::find()
        .filter(
            Condition::all()
                .add(Column::EndedAt.is_null())
                .add(Column::VirusTime.lte(now - COOLDOWN))
                .add(Column::VirusTime.gt(now - COOLDOWN - DURATION)),
        )
        .all(conn)
        .await
}

/// the portal faction when polling starts
pub async fn set_faction<C: ConnectionTrait>(conn: &C, id: i64, faction: Faction) -> Result<(), DbErr> {
    ActiveModel { id: ActiveValue::Unchanged(id), faction: ActiveValue::Set(Some(faction)), ..Default::default() }
        .update(conn)
        .await
        .map(drop)
}

pub async fn end<C: ConnectionTrait>(conn: &C, id: i64, at: i64) -> Result<(), DbErr> {
    ActiveModel { id: ActiveValue::Unchanged(id), ended_at: ActiveValue::Set(Some(at)), ..Default::default() }
        .update(conn)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::testing::db;

    const T: i64 = 1_782_864_000_000;
    const MIN: i64 = 60_000;

    async fn watch(conn: &DatabaseConnection, lat: f64) -> Model {
        Entity::find().filter(Column::Latitude.eq(lat)).one(conn).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn viruses() {
        let Some(db) = db().await else { return };
        let conn = &db.conn;

        assert_eq!(on_virus(conn, 45.1, 11.1, T).await.unwrap(), Virus::Started);
        // later comm polls see it again, maybe with another time
        assert_eq!(on_virus(conn, 45.1, 11.1, T).await.unwrap(), Virus::Known);
        assert_eq!(on_virus(conn, 45.1, 11.1, T + MIN).await.unwrap(), Virus::Known);
        assert_eq!(on_virus(conn, 45.1, 11.1, T - MIN).await.unwrap(), Virus::Known);
        // other portals are independent
        assert_eq!(on_virus(conn, 45.2, 11.1, T + 5 * MIN).await.unwrap(), Virus::Started);
        assert_eq!(Entity::find().count(conn).await.unwrap(), 2);

        assert!(active(conn, T + 59 * MIN).await.unwrap().is_empty());
        assert_eq!(active(conn, T + 60 * MIN).await.unwrap().len(), 1);
        assert_eq!(active(conn, T + 65 * MIN).await.unwrap().len(), 2);

        // second virus
        assert_eq!(on_virus(conn, 45.1, 11.1, T + 90 * MIN).await.unwrap(), Virus::Second);
        assert_eq!(watch(conn, 45.1).await.ended_at, Some(T + 90 * MIN));
        assert_eq!(on_virus(conn, 45.1, 11.1, T + 91 * MIN).await.unwrap(), Virus::Known);
        assert_eq!(active(conn, T + 95 * MIN).await.unwrap().len(), 1);
        // a new virus later starts again
        assert_eq!(on_virus(conn, 45.1, 11.1, T + 300 * MIN).await.unwrap(), Virus::Started);
        let w = watch(conn, 45.1).await;
        assert_eq!((w.virus_time, w.faction, w.ended_at), (T + 300 * MIN, None, None));
    }

    #[tokio::test]
    async fn ended_by_poll_and_expiry() {
        let Some(db) = db().await else { return };
        let conn = &db.conn;

        on_virus(conn, 45.1, 11.1, T).await.unwrap();
        let w = watch(conn, 45.1).await;
        set_faction(conn, w.id, Faction::Resistance).await.unwrap();
        assert_eq!(watch(conn, 45.1).await.faction, Some(Faction::Resistance));
        // a poll saw the faction change before the comm showed the second virus
        end(conn, w.id, T + 100 * MIN).await.unwrap();
        assert!(active(conn, T + 101 * MIN).await.unwrap().is_empty());
        assert_eq!(on_virus(conn, 45.1, 11.1, T + 99 * MIN).await.unwrap(), Virus::Known);

        // expired watches aren't polled, a virus after them starts again
        on_virus(conn, 45.3, 11.1, T).await.unwrap();
        assert_eq!(active(conn, T + COOLDOWN + DURATION - 1).await.unwrap().len(), 1);
        assert!(active(conn, T + COOLDOWN + DURATION).await.unwrap().is_empty());
        assert_eq!(on_virus(conn, 45.3, 11.1, T + COOLDOWN + DURATION).await.unwrap(), Virus::Started);
    }
}
