use ingress_intel_rs::entities;
use sea_orm::{ActiveValue, QuerySelect, SelectColumns, TransactionTrait, entity::prelude::*};
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
    pub async fn get_latest_revision<C: ConnectionTrait>(
        conn: &C,
        portal_id: &str,
    ) -> Result<Option<portal_revision::Model>, DbErr> {
        let Some(Some(revision_id)) = Entity::find()
            .filter(Column::PortalId.eq(portal_id))
            .select_only()
            .select_column(Column::LatestRevisionId)
            .into_tuple::<Option<i64>>()
            .one(conn)
            .await?
        else {
            return Ok(None);
        };

        portal_revision::Entity::find_by_id(revision_id).one(conn).await
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

    for r#mod in portal.mods.iter().flatten().flatten() {
        portal_mods::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            mod_owner: ActiveValue::Set(r#mod.owner.to_string()),
            mod_name: ActiveValue::Set(r#mod.name.to_string()),
            mod_rarity: ActiveValue::Set(r#mod.rarity.to_string()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }

    for reso in portal.resonators.iter().flatten() {
        portal_resonators::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            reso_owner: ActiveValue::Set(reso.owner.to_string()),
            reso_level: ActiveValue::Set(i64::from(reso.level)),
            reso_energy: ActiveValue::Set(i64::from(reso.energy)),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }

    txn.commit().await
}
