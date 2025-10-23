use ingress_intel_rs::entities;
use sea_orm::entity::prelude::*;

// use super::{portal_mods, portal_resonators};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_revisions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub portal_id: i64,
    pub revision: i64,
    pub timestamp: i64,
    pub faction: Faction,
    pub level: i64,
    pub health: i64,
    pub res_count: i64,
    pub owner: Option<String>,
}

// impl Model {
//     pub async fn mods<C: ConnectionTrait>(&self, conn: &C) -> Result<Vec<portal_mods::Model>, DbErr> {
//         portal_mods::Entity::find().filter(portal_mods::Column::RevisionId.eq(self.id)).all(conn).await
//     }

//     pub async fn resonators<C: ConnectionTrait>(&self, conn: &C) -> Result<Vec<portal_resonators::Model>, DbErr> {
//         portal_resonators::Entity::find().filter(portal_resonators::Column::RevisionId.eq(self.id)).all(conn).await
//     }
// }

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(1))")]
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
