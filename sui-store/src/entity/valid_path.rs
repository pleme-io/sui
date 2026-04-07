//! ValidPaths entity — every path registered in the Nix store.

use sea_orm::entity::prelude::*;

/// A registered store path row in the Nix SQLite database.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "ValidPaths")]
pub struct Model {
    /// Auto-incrementing primary key.
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// Full absolute store path (unique).
    #[sea_orm(unique)]
    pub path: String,
    /// SHA-256 hash of the NAR, in Nix's `sha256:<base16>` format.
    pub hash: String,
    /// Unix timestamp of registration.
    #[sea_orm(column_name = "registrationTime")]
    pub registration_time: i64,
    /// Store path of the `.drv` that produced this path.
    pub deriver: Option<String>,
    /// Size of the NAR archive in bytes.
    #[sea_orm(column_name = "narSize")]
    pub nar_size: Option<i64>,
    /// 1 if built locally (not substituted).
    pub ultimate: Option<i32>,
    /// Space-separated Ed25519 signatures.
    pub sigs: Option<String>,
    /// Content-address assertion (e.g., `fixed:out:r:sha256:...`).
    pub ca: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::reference::Entity")]
    References,
    #[sea_orm(has_many = "super::derivation_output::Entity")]
    DerivationOutputs,
}

impl Related<super::reference::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::References.def()
    }
}

impl Related<super::derivation_output::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivationOutputs.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
