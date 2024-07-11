use ingress_intel_rs::{entities, portal_details};
use sea_orm::{entity::prelude::*, ActiveValue, QuerySelect, SelectColumns, TransactionTrait};
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
    pub async fn get_latest_revision<C: ConnectionTrait>(
        &self,
        conn: &C,
    ) -> Result<Option<portal_revision::Model>, DbErr> {
        portal_revision::Entity::find_by_id(self.latest_revision_id).one(conn).await
    }

    pub fn as_portal(&self) -> Portal<'_> {
        Portal {
            name: &self.title,
            address: "maps",
            lat: Decimal::from_f64_retain(self.latitude).expect("invalid latitude"),
            lon: Decimal::from_f64_retain(self.longitude).expect("invalid longitude"),
        }
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

pub async fn should_scan<C: ConnectionTrait>(conn: &C, portal: &entities::IntelEntity) -> bool {
    let Some(id) = portal.get_id() else {
        error!("Portal without id {portal:?}");
        return false;
    };
    let Some(faction) = portal.get_faction() else {
        error!("Portal without faction {portal:?}");
        return false;
    };
    // let Some(latitude) = portal.get_latitude() else {
    //     error!("Portal without latitude {portal:?}");
    //     return false;
    // };
    // let Some(longitude) = portal.get_longitude() else {
    //     error!("Portal without longitude {portal:?}");
    //     return false;
    // };
    let Some(level) = portal.get_level() else {
        error!("Portal without level {portal:?}");
        return false;
    };
    let Some(health) = portal.get_health() else {
        error!("Portal without health {portal:?}");
        return false;
    };
    let Some(res_count) = portal.get_res_count() else {
        error!("Portal without resCount {portal:?}");
        return false;
    };
    // let image = portal.get_image();
    // let Some(title) = portal.get_name() else {
    //     error!("Portal without title {portal:?}");
    //     return false;
    // };
    // let owner = portal.get_owner();

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
    } = match Entity::get_latest_revision(conn, id).await {
        Ok(Some(model)) => model,
        Ok(None) => return true,
        Err(err) => {
            error!("Database error: {err}");
            return false;
        }
    };

    portal_revision::Faction::from(faction) != old_faction
        || level != old_level as u8
        || health != old_health as u8
        || res_count != old_res_count as u8
}

pub async fn update_or_insert<C: TransactionTrait>(
    conn: &C,
    id: &str,
    timestamp: i64,
    old_model: Option<portal_revision::Model>,
    portal: portal_details::IntelPortal,
) -> Result<(), DbErr> {
    let txn = conn.begin().await?;

    let portal_model = if let Some(old_model) = &old_model {
        let Some(portal_model) = Entity::find_by_id(old_model.portal_id).one(&txn).await? else {
            return Err(DbErr::RecordNotFound(format!("Can't find portal with id {}", old_model.portal_id)));
        };

        if portal_model.latitude != portal.get_latitude()
            || portal_model.longitude != portal.get_longitude()
            || portal_model.image.as_deref() != portal.get_image()
            || portal_model.title != portal.get_title()
        {
            ActiveModel {
                id: ActiveValue::Set(portal_model.id),
                portal_id: ActiveValue::Set(id.to_owned()),
                latest_revision_id: ActiveValue::Set(portal_model.latest_revision_id),
                latitude: ActiveValue::Set(portal.get_latitude()),
                longitude: ActiveValue::Set(portal.get_longitude()),
                image: ActiveValue::Set(portal.get_image().map(ToOwned::to_owned)),
                title: ActiveValue::Set(portal.get_title().to_owned()),
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
            latitude: ActiveValue::Set(portal.get_latitude()),
            longitude: ActiveValue::Set(portal.get_longitude()),
            image: ActiveValue::Set(portal.get_image().map(ToOwned::to_owned)),
            title: ActiveValue::Set(portal.get_title().to_owned()),
            ..Default::default()
        }
        .insert(&txn)
        .await?
    };

    let revision_model = portal_revision::ActiveModel {
        portal_id: ActiveValue::Set(portal_model.id),
        revision: ActiveValue::Set(old_model.as_ref().map(|model| model.revision + 1).unwrap_or_default()),
        timestamp: ActiveValue::Set(timestamp),
        faction: ActiveValue::Set(portal_revision::Faction::from(portal.get_faction().expect("missing faction"))),
        level: ActiveValue::Set(i32::from(portal.get_level())),
        health: ActiveValue::Set(i32::from(portal.get_health())),
        res_count: ActiveValue::Set(i32::from(portal.get_res_count())),
        owner: ActiveValue::Set(portal.get_owner().map(ToOwned::to_owned)),
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

    for r#mod in portal.get_mods().iter().flatten() {
        portal_mods::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            mod_owner: ActiveValue::Set(r#mod.get_owner().to_owned()),
            mod_name: ActiveValue::Set(r#mod.get_name().to_owned()),
            mod_rarity: ActiveValue::Set(r#mod.get_rarity().to_owned()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }

    for reso in portal.get_resonators().iter().flatten() {
        portal_resonators::ActiveModel {
            revision_id: ActiveValue::Set(revision_model.id),
            reso_owner: ActiveValue::Set(reso.get_owner().to_owned()),
            reso_level: ActiveValue::Set(i32::from(reso.get_level())),
            reso_energy: ActiveValue::Set(i32::from(reso.get_energy())),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
    }

    txn.commit().await
}
