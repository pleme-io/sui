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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveValue, EntityName, IntoActiveModel};

    fn sample_model() -> Model {
        Model {
            drv: 42,
            id: "out".to_string(),
            path: "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1".to_string(),
        }
    }

    #[test]
    fn model_field_access() {
        let m = sample_model();
        assert_eq!(m.drv, 42);
        assert_eq!(m.id, "out");
        assert!(m.path.contains("hello"));
    }

    #[test]
    fn model_clone_independence() {
        let original = sample_model();
        let mut cloned = original.clone();
        cloned.drv = 99;
        cloned.id = "dev".to_string();
        assert_eq!(original.drv, 42);
        assert_eq!(original.id, "out");
        assert_eq!(cloned.drv, 99);
        assert_eq!(cloned.id, "dev");
    }

    #[test]
    fn model_eq_full_match() {
        let a = sample_model();
        let b = sample_model();
        assert_eq!(a, b);
    }

    #[test]
    fn model_inequality_drv_differs() {
        let a = sample_model();
        let mut b = sample_model();
        b.drv = 99;
        assert_ne!(a, b);
    }

    #[test]
    fn model_inequality_id_differs() {
        let a = sample_model();
        let mut b = sample_model();
        b.id = "lib".to_string();
        assert_ne!(a, b);
    }

    #[test]
    fn model_inequality_path_differs() {
        let a = sample_model();
        let mut b = sample_model();
        b.path = "/nix/store/different".to_string();
        assert_ne!(a, b);
    }

    #[test]
    fn model_debug_format() {
        let m = sample_model();
        let debug = format!("{m:?}");
        assert!(debug.contains("hello"));
        assert!(debug.contains("out"));
    }

    #[test]
    fn model_with_dev_output() {
        let m = Model {
            drv: 1,
            id: "dev".to_string(),
            path: "/nix/store/abc-pkg-dev".to_string(),
        };
        assert_eq!(m.id, "dev");
        assert!(m.path.ends_with("-dev"));
    }

    #[test]
    fn model_with_lib_output() {
        let m = Model {
            drv: 1,
            id: "lib".to_string(),
            path: "/nix/store/abc-pkg-lib".to_string(),
        };
        assert_eq!(m.id, "lib");
    }

    #[test]
    fn model_with_man_output() {
        let m = Model {
            drv: 1,
            id: "man".to_string(),
            path: "/nix/store/abc-pkg-man".to_string(),
        };
        assert_eq!(m.id, "man");
    }

    #[test]
    fn model_into_active_model_carries_pks() {
        let m = sample_model();
        let active: ActiveModel = m.clone().into_active_model();
        match active.drv {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, 42),
            ActiveValue::NotSet => panic!("drv should be set"),
        }
        match active.id {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, "out"),
            ActiveValue::NotSet => panic!("id should be set"),
        }
    }

    #[test]
    fn entity_table_name_is_derivation_outputs() {
        assert_eq!(Entity.table_name(), "DerivationOutputs");
    }

    #[test]
    fn relation_definition_compiles() {
        let _def = Relation::ValidPath.def();
    }

    #[test]
    fn related_to_valid_path_compiles() {
        use sea_orm::Related;
        let _ = <Entity as Related<super::super::valid_path::Entity>>::to();
    }

    #[test]
    fn model_with_empty_id_string() {
        let m = Model {
            drv: 1,
            id: String::new(),
            path: "/nix/store/abc".to_string(),
        };
        assert!(m.id.is_empty());
    }

    #[test]
    fn model_with_unusual_output_name() {
        // Custom output names are allowed
        let m = Model {
            drv: 1,
            id: "doc-pdf".to_string(),
            path: "/nix/store/abc-pkg-doc-pdf".to_string(),
        };
        assert_eq!(m.id, "doc-pdf");
    }
}
