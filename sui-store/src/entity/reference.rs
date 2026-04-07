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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveValue, EntityName, IntoActiveModel};

    #[test]
    fn model_field_access() {
        let m = Model {
            referrer: 7,
            reference: 14,
        };
        assert_eq!(m.referrer, 7);
        assert_eq!(m.reference, 14);
    }

    #[test]
    fn model_clone_independence() {
        let original = Model {
            referrer: 1,
            reference: 2,
        };
        let mut cloned = original.clone();
        cloned.referrer = 99;
        assert_eq!(original.referrer, 1);
        assert_eq!(cloned.referrer, 99);
    }

    #[test]
    fn model_equality_full_match() {
        let a = Model {
            referrer: 5,
            reference: 10,
        };
        let b = Model {
            referrer: 5,
            reference: 10,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn model_inequality_referrer_differs() {
        let a = Model {
            referrer: 5,
            reference: 10,
        };
        let b = Model {
            referrer: 6,
            reference: 10,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn model_inequality_reference_differs() {
        let a = Model {
            referrer: 5,
            reference: 10,
        };
        let b = Model {
            referrer: 5,
            reference: 11,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn model_debug_format() {
        let m = Model {
            referrer: 100,
            reference: 200,
        };
        let debug = format!("{m:?}");
        assert!(debug.contains("100"));
        assert!(debug.contains("200"));
    }

    #[test]
    fn model_into_active_model_carries_pks() {
        let m = Model {
            referrer: 42,
            reference: 99,
        };
        let active: ActiveModel = m.clone().into_active_model();
        match active.referrer {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, 42),
            ActiveValue::NotSet => panic!("referrer should be set"),
        }
        match active.reference {
            ActiveValue::Unchanged(v) | ActiveValue::Set(v) => assert_eq!(v, 99),
            ActiveValue::NotSet => panic!("reference should be set"),
        }
    }

    #[test]
    fn entity_table_name_is_refs() {
        assert_eq!(Entity.table_name(), "Refs");
    }

    #[test]
    fn relation_definition_compiles() {
        let _def = Relation::Referrer.def();
    }

    #[test]
    fn related_to_valid_path_compiles() {
        use sea_orm::Related;
        let _ = <Entity as Related<super::super::valid_path::Entity>>::to();
    }

    #[test]
    fn model_with_zero_ids() {
        let m = Model {
            referrer: 0,
            reference: 0,
        };
        assert_eq!(m.referrer, 0);
        assert_eq!(m.reference, 0);
    }

    #[test]
    fn model_with_max_ids() {
        let m = Model {
            referrer: i64::MAX,
            reference: i64::MAX,
        };
        assert_eq!(m.referrer, i64::MAX);
        assert_eq!(m.reference, i64::MAX);
    }

    #[test]
    fn model_self_reference_allowed_at_type_level() {
        // The schema allows a path to reference itself (loops happen)
        let m = Model {
            referrer: 5,
            reference: 5,
        };
        assert_eq!(m.referrer, m.reference);
    }
}
