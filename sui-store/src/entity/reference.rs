//! Refs entity — dependency edges between store paths.

use sea_orm::entity::prelude::*;

/// A dependency edge between two store paths.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "Refs")]
pub struct Model {
    /// FK to ValidPaths.id — the path that references another.
    #[sea_orm(primary_key, auto_increment = false)]
    pub referrer: i64,
    /// FK to ValidPaths.id — the path being referenced.
    #[sea_orm(primary_key, auto_increment = false)]
    pub reference: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::valid_path::Entity",
        from = "Column::Referrer",
        to = "super::valid_path::Column::Id"
    )]
    Referrer,
}

impl Related<super::valid_path::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Referrer.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
