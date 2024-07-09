use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_mods")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub revision_id: i64,
    pub mod_owner: String,
    pub mod_name: String,
    pub mod_rarity: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
