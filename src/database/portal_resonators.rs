use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "portal_resonators")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub portal_id: String,
    pub portal_revision: i64,
    pub reso_owner: String,
    pub reso_level: i32,
    pub reso_energy: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
