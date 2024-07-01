use ingress_intel_rs::{entities, portal_details};
use sea_orm::{entity::prelude::*, ActiveValue, QuerySelect, SelectColumns};
use tracing::error;

use super::{portal_mods, portal_resonators};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portals")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub revision: i64,
    pub timestamp: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub faction: Faction,
    pub level: i32,
    pub health: i32,
    pub res_count: i32,
    pub image: Option<String>,
    pub title: String,
    pub owner: Option<String>,
}

impl Model {
    pub async fn mods<C: ConnectionTrait>(&self, conn: &C) -> Result<Vec<portal_mods::Model>, DbErr> {
        portal_mods::Entity::find()
            .filter(
                portal_mods::Column::PortalId
                    .eq(self.id.as_str())
                    .and(portal_mods::Column::PortalRevision.eq(self.revision)),
            )
            .all(conn)
            .await
    }

    pub async fn resonators<C: ConnectionTrait>(&self, conn: &C) -> Result<Vec<portal_resonators::Model>, DbErr> {
        portal_resonators::Entity::find()
            .filter(
                portal_resonators::Column::PortalId
                    .eq(self.id.as_str())
                    .and(portal_resonators::Column::PortalRevision.eq(self.revision)),
            )
            .all(conn)
            .await
    }
}

impl Entity {
    pub async fn get_latest_revision<C: ConnectionTrait>(conn: &C, id: &str) -> Result<Option<Model>, DbErr> {
        let Some(Some(revision)) = Entity::find()
            .filter(Column::Id.eq(id))
            .select_only()
            .select_column_as(Column::Revision.max(), "revision")
            .into_tuple::<Option<i64>>()
            .one(conn)
            .await?
        else {
            return Ok(None);
        };

        Entity::find_by_id((id.to_owned(), revision)).one(conn).await
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(Some(1))")]
pub enum Faction {
    #[sea_orm(string_value = "N")]
    Neutral,
    #[sea_orm(string_value = "E")]
    Enlightened,
    #[sea_orm(string_value = "R")]
    Resistance,
    #[sea_orm(string_value = "M")]
    Machina,
}

impl From<entities::Faction> for Faction {
    fn from(value: ingress_intel_rs::entities::Faction) -> Self {
        match value {
            entities::Faction::Neutral => Faction::Neutral,
            entities::Faction::Enlightened => Faction::Enlightened,
            entities::Faction::Resistance => Faction::Resistance,
            entities::Faction::Machina => Faction::Machina,
        }
    }
}

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
    let Model {
        id: _,
        revision: _,
        timestamp: _,
        latitude: _,
        longitude: _,
        faction: old_faction,
        level: old_level,
        health: old_health,
        res_count: old_res_count,
        image: _,
        title: _,
        owner: _,
    } = match Entity::get_latest_revision(conn, id).await {
        Ok(Some(model)) => model,
        Ok(None) => return true,
        Err(err) => {
            error!("Database error: {err}");
            return false;
        }
    };

    Faction::from(faction) != old_faction
        || level != old_level as u8
        || health != old_health as u8
        || res_count != old_res_count as u8
}

pub async fn update_or_insert<C: ConnectionTrait>(
    conn: &C,
    id: &str,
    timestamp: i64,
    old_model: Option<Model>,
    portal: portal_details::IntelPortal,
) -> Result<(), DbErr> {
    let model = ActiveModel {
        id: ActiveValue::Set(id.to_owned()),
        revision: ActiveValue::Set(old_model.as_ref().map(|model| model.revision + 1).unwrap_or_default()),
        timestamp: ActiveValue::Set(timestamp),
        latitude: ActiveValue::Set(portal.get_latitude()),
        longitude: ActiveValue::Set(portal.get_longitude()),
        faction: ActiveValue::Set(Faction::from(portal.get_faction().expect("missing faction"))),
        level: ActiveValue::Set(i32::from(portal.get_level())),
        health: ActiveValue::Set(i32::from(portal.get_health())),
        res_count: ActiveValue::Set(i32::from(portal.get_res_count())),
        image: ActiveValue::Set(portal.get_image().map(ToOwned::to_owned)),
        title: ActiveValue::Set(portal.get_title().to_owned()),
        owner: ActiveValue::Set(portal.get_owner().map(ToOwned::to_owned)),
    }
    .insert(conn)
    .await?;

    for r#mod in portal.get_mods().iter().flatten() {
        portal_mods::ActiveModel {
            portal_id: ActiveValue::Set(model.id.clone()),
            portal_revision: ActiveValue::Set(model.revision),
            mod_owner: ActiveValue::Set(r#mod.get_owner().to_owned()),
            mod_name: ActiveValue::Set(r#mod.get_name().to_owned()),
            mod_rarity: ActiveValue::Set(r#mod.get_rarity().to_owned()),
            ..Default::default()
        }
        .insert(conn)
        .await?;
    }

    for reso in portal.get_resonators().iter().flatten() {
        portal_resonators::ActiveModel {
            portal_id: ActiveValue::Set(model.id.clone()),
            portal_revision: ActiveValue::Set(model.revision),
            reso_owner: ActiveValue::Set(reso.get_owner().to_owned()),
            reso_level: ActiveValue::Set(i32::from(reso.get_level())),
            reso_energy: ActiveValue::Set(i32::from(reso.get_energy())),
            ..Default::default()
        }
        .insert(conn)
        .await?;
    }

    Ok(())
}
