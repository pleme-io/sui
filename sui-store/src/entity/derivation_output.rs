//! DerivationOutputs entity — maps derivation outputs to store paths.

use sea_orm::entity::prelude::*;

/// A mapping from derivation output to its store path.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "DerivationOutputs")]
pub struct Model {
    /// FK to ValidPaths.id of the `.drv` path.
    #[sea_orm(primary_key, auto_increment = false)]
    pub drv: i64,
    /// Output name (typically "out", "dev", "lib", etc.).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Output store path.
    pub path: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::valid_path::Entity",
        from = "Column::Drv",
        to = "super::valid_path::Column::Id"
    )]
    ValidPath,
}

impl Related<super::valid_path::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ValidPath.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
