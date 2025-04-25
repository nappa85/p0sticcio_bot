use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_resonators")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub revision_id: i64,
    pub reso_owner: String,
    pub reso_level: i64,
    pub reso_energy: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
