use ingress_intel_rs::entities;
use sea_orm::{ActiveValue, ExprTrait, Statement, TransactionSession, TransactionTrait, entity::prelude::*};
use tracing::error;

use crate::entities::Portal;

use super::{portal_mods, portal_resonators, portal_revision};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portals")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub portal_id: String,
    pub latest_revision_id: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub image: Option<String>,
    pub title: String,
}

impl Model {
    // pub async fn get_latest_revision<C: ConnectionTrait>(
    //     &self,
    //     conn: &C,
    // ) -> Result<Option<portal_revision::Model>, DbErr> {
    //     portal_revision::Entity::find_by_id(self.latest_revision_id).one(conn).await
    // }

    pub fn as_portal(&self) -> Portal<&'_ str> {
        Portal { name: &self.title, address: "maps", lat: self.latitude, lon: self.longitude }
    }
}

impl Entity {
    pub async fn find_by_coords<C: ConnectionTrait>(conn: &C, lat: f64, lon: f64) -> Result<Option<Model>, DbErr> {
        Entity::find().filter(Column::Latitude.eq(lat).and(Column::Longitude.eq(lon))).one(conn).await
    }

    pub async fn get_latest_revision<C: ConnectionTrait>(
        conn: &C,
        portal_id: &str,
    ) -> Result<Option<portal_revision::Model>, DbErr> {
        // single round trip, this runs for every portal seen on every map scan
        portal_revision::Entity::find()
            .from_raw_sql(Statement::from_sql_and_values(
                conn.get_database_backend(),
                "select pr.* from portals p inner join portal_revisions pr on pr.id = p.latest_revision_id where p.portal_id = $1",
                [portal_id.into()],
            ))
            .one(conn)
            .await
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub async fn should_scan<C: ConnectionTrait>(conn: &C, portal: &entities::Entity<entities::IntelPortal>) -> bool {
    // at this point we don't care if the portal has been moved or renamed
    let portal_revision::Model {
        id: _,
        portal_id: _,
        revision: _,
        timestamp: _,
        faction: old_faction,
        level: old_level,
        health: old_health,
        res_count: old_res_count,
        owner: _,
    } = match Entity::get_latest_revision(conn, &portal.id).await {
        Ok(Some(model)) => model,
        Ok(None) => return true,
        Err(err) => {
            error!("Database error: {err}");
            return false;
        }
    };

    portal_revision::Faction::from(portal.entity.faction) != old_faction
        || portal.entity.level != old_level as u8
        || portal.entity.health != old_health as u8
        || portal.entity.res_count != old_res_count as u8
}

/// whether the portal differs from the stored revision, ignoring health and resonators energy that change all the time
pub async fn differs<C: ConnectionTrait>(
    conn: &C,
    old: &portal_revision::Model,
    portal: &entities::IntelPortal,
) -> Result<bool, DbErr> {
    if portal_revision::Faction::from(portal.faction) != old.faction
        || i64::from(portal.level) != old.level
        || i64::from(portal.res_count) != old.res_count
        || portal.owner.as_deref() != old.owner.as_deref()
    {
        return Ok(true);
    }

    let mut old_resonators = portal_resonators::Entity::find()
        .filter(portal_resonators::Column::RevisionId.eq(old.id))
        .all(conn)
        .await?
        .into_iter()
        .map(|reso| (reso.reso_owner, reso.reso_level))
        .collect::<Vec<_>>();
    let mut new_resonators = portal
        .resonators
        .iter()
        .flatten()
        .map(|reso| (reso.owner.to_string(), i64::from(reso.level)))
        .collect::<Vec<_>>();
    old_resonators.sort();
    new_resonators.sort();
    if old_resonators != new_resonators {
        return Ok(true);
    }

    let mut old_mods = portal_mods::Entity::find()
        .filter(portal_mods::Column::RevisionId.eq(old.id))
        .all(conn)
        .await?
        .into_iter()
        .map(|r#mod| (r#mod.mod_owner, r#mod.mod_name, r#mod.mod_rarity))
        .collect::<Vec<_>>();
    let mut new_mods = portal
        .mods
        .iter()
        .flatten()
        .flatten()
        .map(|r#mod| (r#mod.owner.to_string(), r#mod.name.to_string(), r#mod.rarity.to_string()))
        .collect::<Vec<_>>();
    old_mods.sort();
    new_mods.sort();
    Ok(old_mods != new_mods)
}

pub async fn update_or_insert<C: TransactionTrait>(
    conn: &C,
    id: &str,
    timestamp: i64,
    old_model: Option<portal_revision::Model>,
    portal: entities::IntelPortal,
) -> Result<(), DbErr> {
    let txn = conn.begin().await?;

    let portal_model = if let Some(old_model) = &old_model {
        let Some(portal_model) = Entity::find_by_id(old_model.portal_id).one(&txn).await? else {
            return Err(DbErr::RecordNotFound(format!("Can't find portal with id {}", old_model.portal_id)));
        };

        if portal_model.latitude != portal.latitude
            || portal_model.longitude != portal.longitude
            || portal_model.image.as_deref() != portal.image.as_deref()
            || portal_model.title != portal.title
        {
            ActiveModel {
                id: ActiveValue::Set(portal_model.id),
                portal_id: ActiveValue::Set(id.to_owned()),
                latest_revision_id: ActiveValue::Set(portal_model.latest_revision_id),
                latitude: ActiveValue::Set(portal.latitude),
                longitude: ActiveValue::Set(portal.longitude),
                image: ActiveValue::Set(portal.image.as_ref().map(ToString::to_string)),
                title: ActiveValue::Set(portal.title.to_string()),
            }
            .update(&txn)
            .await?
        } else {
            portal_model
        }
    } else {
        ActiveModel {
            portal_id: ActiveValue::Set(id.to_owned()),
            latest_revision_id: ActiveValue::Set(0),
            latitude: ActiveValue::Set(portal.latitude),
            longitude: ActiveValue::Set(portal.longitude),
            image: ActiveValue::Set(portal.image.as_ref().map(ToString::to_string)),
            title: ActiveValue::Set(portal.title.to_string()),
            ..Default::default()
        }
        .insert(&txn)
        .await?
    };

    let revision_model = portal_revision::ActiveModel {
        portal_id: ActiveValue::Set(portal_model.id),
        revision: ActiveValue::Set(old_model.as_ref().map(|model| model.revision + 1).unwrap_or_default()),
        timestamp: ActiveValue::Set(timestamp),
        faction: ActiveValue::Set(portal_revision::Faction::from(portal.faction)),
        level: ActiveValue::Set(i64::from(portal.level)),
        health: ActiveValue::Set(i64::from(portal.health)),
        res_count: ActiveValue::Set(i64::from(portal.res_count)),
        owner: ActiveValue::Set(portal.owner.as_ref().map(ToString::to_string)),
        ..Default::default()
    }
    .insert(&txn)
    .await?;

    ActiveModel {
        id: ActiveValue::Set(portal_model.id),
        latest_revision_id: ActiveValue::Set(revision_model.id),
        ..Default::default()
    }
    .update(&txn)
    .await?;

    let mods = portal
        .mods
        .iter()
        .flatten()
        .flatten()
        .map(|r#mod| portal_mods::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            mod_owner: ActiveValue::Set(r#mod.owner.to_string()),
            mod_name: ActiveValue::Set(r#mod.name.to_string()),
            mod_rarity: ActiveValue::Set(r#mod.rarity.to_string()),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    if !mods.is_empty() {
        portal_mods::Entity::insert_many(mods).exec_without_returning(&txn).await?;
    }

    let resonators = portal
        .resonators
        .iter()
        .flatten()
        .map(|reso| portal_resonators::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            reso_owner: ActiveValue::Set(reso.owner.to_string()),
            reso_level: ActiveValue::Set(i64::from(reso.level)),
            reso_energy: ActiveValue::Set(i64::from(reso.energy)),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    if !resonators.is_empty() {
        portal_resonators::Entity::insert_many(resonators).exec_without_returning(&txn).await?;
    }

    txn.commit().await
}

#[cfg(test)]
mod tests {
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder};

    use super::*;
    use crate::database::testing::{Spec, db, intel_portal};

    #[tokio::test]
    async fn insert_and_update() {
        let Some(db) = db().await else { return };
        let conn = &db.conn;

        assert_eq!(Entity::get_latest_revision(conn, "pid.1").await.unwrap(), None);
        assert_eq!(Entity::find_by_coords(conn, 45.123456, 11.654321).await.unwrap(), None);

        let spec = Spec {
            title: "Fontana",
            faction: "E",
            lat: 45.123456,
            lon: 11.654321,
            level: 7,
            health: 90,
            owner: Some("alice"),
            resonators: &[("alice", 8, 6000), ("bob", 7, 5000)],
            mods: &[("alice", "Heat Sink", "VERY_RARE"), ("bob", "Portal Shield", "COMMON")],
        };
        update_or_insert(conn, "pid.1", 1_000, None, intel_portal(spec)).await.unwrap();

        let rev0 = Entity::get_latest_revision(conn, "pid.1").await.unwrap().expect("missing revision");
        let portal = Entity::find_by_coords(conn, 45.123456, 11.654321).await.unwrap().expect("missing portal");
        assert_eq!(
            portal,
            Model {
                id: portal.id,
                portal_id: String::from("pid.1"),
                latest_revision_id: rev0.id,
                latitude: 45.123456,
                longitude: 11.654321,
                image: Some(String::from("https://example.com/image.jpg")),
                title: String::from("Fontana"),
            }
        );
        assert_eq!(
            rev0,
            portal_revision::Model {
                id: rev0.id,
                portal_id: portal.id,
                revision: 0,
                timestamp: 1_000,
                faction: portal_revision::Faction::Enlightened,
                level: 7,
                health: 90,
                res_count: 2,
                owner: Some(String::from("alice")),
            }
        );

        let mods = portal_mods::Entity::find()
            .filter(portal_mods::Column::RevisionId.eq(rev0.id))
            .order_by_asc(portal_mods::Column::Id)
            .all(conn)
            .await
            .unwrap()
            .into_iter()
            .map(|m| (m.mod_owner, m.mod_name, m.mod_rarity))
            .collect::<Vec<_>>();
        assert_eq!(
            mods,
            [
                (String::from("alice"), String::from("Heat Sink"), String::from("VERY_RARE")),
                (String::from("bob"), String::from("Portal Shield"), String::from("COMMON"))
            ]
        );
        let resos = portal_resonators::Entity::find()
            .filter(portal_resonators::Column::RevisionId.eq(rev0.id))
            .order_by_asc(portal_resonators::Column::Id)
            .all(conn)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.reso_owner, r.reso_level, r.reso_energy))
            .collect::<Vec<_>>();
        assert_eq!(resos, [(String::from("alice"), 8, 6000), (String::from("bob"), 7, 5000)]);

        // same place, renamed and captured
        let spec = Spec {
            title: "Fontana Nuova",
            faction: "M",
            lat: 45.123456,
            lon: 11.654321,
            level: 1,
            health: 100,
            owner: Some("__MACHINA__"),
            ..Default::default()
        };
        update_or_insert(conn, "pid.1", 2_000, Some(rev0.clone()), intel_portal(spec)).await.unwrap();

        let rev1 = Entity::get_latest_revision(conn, "pid.1").await.unwrap().expect("missing revision");
        assert_eq!(rev1.revision, 1);
        assert_eq!(rev1.timestamp, 2_000);
        assert_eq!(rev1.faction, portal_revision::Faction::Machina);
        assert_eq!(rev1.res_count, 0);
        assert_ne!(rev1.id, rev0.id);
        let updated = Entity::find_by_id(portal.id).one(conn).await.unwrap().expect("missing portal");
        assert_eq!(updated.title, "Fontana Nuova");
        assert_eq!(updated.latest_revision_id, rev1.id);
        assert_eq!(Entity::find().count(conn).await.unwrap(), 1);
        assert_eq!(portal_revision::Entity::find().count(conn).await.unwrap(), 2);
        assert_eq!(
            portal_mods::Entity::find().filter(portal_mods::Column::RevisionId.eq(rev1.id)).count(conn).await.unwrap(),
            0
        );

        // a second portal doesn't interfere
        let spec = Spec { title: "Chiesa", faction: "R", lat: 46.0, lon: 12.0, ..Default::default() };
        update_or_insert(conn, "pid.2", 3_000, None, intel_portal(spec)).await.unwrap();
        let other = Entity::get_latest_revision(conn, "pid.2").await.unwrap().expect("missing revision");
        assert_eq!((other.revision, other.faction, other.owner), (0, portal_revision::Faction::Resistance, None));
        assert_eq!(Entity::get_latest_revision(conn, "pid.1").await.unwrap(), Some(rev1));
    }

    #[tokio::test]
    async fn changes() {
        let Some(db) = db().await else { return };
        let conn = &db.conn;

        const BASE: Spec<'static> = Spec {
            title: "Fontana",
            faction: "R",
            lat: 45.1,
            lon: 11.1,
            level: 6,
            health: 90,
            owner: Some("alice"),
            resonators: &[("alice", 8, 6000), ("bob", 5, 2000), ("bob", 5, 2000)],
            mods: &[("alice", "Heat Sink", "VERY_RARE")],
        };
        update_or_insert(conn, "pid.1", 1_000, None, intel_portal(BASE)).await.unwrap();
        let old = Entity::get_latest_revision(conn, "pid.1").await.unwrap().unwrap();
        let differs = async |spec| differs(conn, &old, &intel_portal(spec)).await.unwrap();

        assert!(!differs(BASE).await);
        // order, health and energy don't matter
        assert!(
            !differs(Spec { health: 40, resonators: &[("bob", 5, 100), ("alice", 8, 10), ("bob", 5, 1000)], ..BASE })
                .await
        );
        // an upgrade by another agent
        assert!(
            differs(Spec { resonators: &[("alice", 8, 6000), ("carol", 8, 6000), ("bob", 5, 2000)], ..BASE }).await
        );
        assert!(differs(Spec { resonators: &[("alice", 8, 6000), ("bob", 6, 2000), ("bob", 5, 2000)], ..BASE }).await);
        assert!(differs(Spec { mods: &[("alice", "Heat Sink", "RARE")], ..BASE }).await);
        assert!(differs(Spec { mods: &[], ..BASE }).await);
        assert!(differs(Spec { faction: "E", ..BASE }).await);
        assert!(differs(Spec { level: 7, ..BASE }).await);
        assert!(differs(Spec { owner: Some("carol"), ..BASE }).await);
    }
}
