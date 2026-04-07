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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveValue, IntoActiveModel};

    fn sample_model() -> Model {
        Model {
            id: 1,
            path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
            hash: "sha256:1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7".to_string(),
            registration_time: 1_700_000_000,
            deriver: Some(
                "/nix/store/xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv".to_string(),
            ),
            nar_size: Some(226552),
            ultimate: Some(0),
            sigs: Some("cache.nixos.org-1:abc==".to_string()),
            ca: None,
        }
    }

    #[test]
    fn model_field_access() {
        let m = sample_model();
        assert_eq!(m.id, 1);
        assert!(m.path.contains("hello"));
        assert!(m.deriver.is_some());
        assert_eq!(m.nar_size, Some(226552));
    }

    #[test]
    fn model_clone_independence() {
        let original = sample_model();
        let mut cloned = original.clone();
        cloned.id = 99;
        cloned.nar_size = Some(0);
        assert_eq!(original.id, 1);
        assert_eq!(original.nar_size, Some(226552));
        assert_eq!(cloned.id, 99);
    }

    #[test]
    fn model_eq_and_hash() {
        let a = sample_model();
        let b = a.clone();
        assert_eq!(a, b);

        let mut c = a.clone();
        c.hash = "sha256:different".to_string();
        assert_ne!(a, c);
    }

    #[test]
    fn model_debug_format_includes_path_and_hash() {
        let m = sample_model();
        let debug = format!("{m:?}");
        assert!(debug.contains("hello"));
        assert!(debug.contains("sha256"));
    }

    #[test]
    fn model_with_minimal_fields() {
        let m = Model {
            id: 0,
            path: "/nix/store/abc-leaf".to_string(),
            hash: "sha256:000".to_string(),
            registration_time: 0,
            deriver: None,
            nar_size: None,
            ultimate: None,
            sigs: None,
            ca: None,
        };
        assert!(m.deriver.is_none());
        assert!(m.nar_size.is_none());
        assert!(m.ultimate.is_none());
        assert!(m.sigs.is_none());
        assert!(m.ca.is_none());
    }

    #[test]
    fn model_with_content_address() {
        let m = Model {
            id: 5,
            path: "/nix/store/abc-fixed".to_string(),
            hash: "sha256:aaa".to_string(),
            registration_time: 100,
            deriver: None,
            nar_size: Some(500),
            ultimate: None,
            sigs: None,
            ca: Some("fixed:out:r:sha256:deadbeef".to_string()),
        };
        assert_eq!(m.ca.as_deref(), Some("fixed:out:r:sha256:deadbeef"));
    }

    #[test]
    fn model_with_ultimate_flag() {
        let mut m = sample_model();
        m.ultimate = Some(1);
        assert_eq!(m.ultimate, Some(1));
        m.ultimate = Some(0);
        assert_eq!(m.ultimate, Some(0));
    }

    #[test]
    fn model_into_active_model_carries_values() {
        let m = sample_model();
        let active: ActiveModel = m.clone().into_active_model();
        // Each field should become Set(value), not NotSet/Unchanged
        match active.id {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, m.id),
            ActiveValue::NotSet => panic!("id should be set"),
        }
        match active.path {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, m.path),
            ActiveValue::NotSet => panic!("path should be set"),
        }
    }

    #[test]
    fn entity_table_name_is_validpaths() {
        use sea_orm::EntityName;
        assert_eq!(Entity.table_name(), "ValidPaths");
    }

    #[test]
    fn relation_definitions_exist() {
        // Both relations must compile and have a Definition
        let _refs_def = Relation::References.def();
        let _drv_outputs_def = Relation::DerivationOutputs.def();
    }

    #[test]
    fn related_to_reference_compiles() {
        // Compile-time check that Related impl exists
        use sea_orm::Related;
        let _ = <Entity as Related<super::super::reference::Entity>>::to();
    }

    #[test]
    fn related_to_derivation_output_compiles() {
        use sea_orm::Related;
        let _ = <Entity as Related<super::super::derivation_output::Entity>>::to();
    }
}
