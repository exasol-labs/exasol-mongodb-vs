use std::collections::{BTreeMap, BTreeSet};

use mongodb::bson::{Bson, Document};
use proptest::collection::{btree_map, vec};
use proptest::prelude::*;

use super::*;

fn field_name() -> impl Strategy<Value = String> {
    vec(
        any::<char>().prop_filter("BSON keys cannot contain NUL", |value| *value != '\0'),
        0..80,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn distributed_array_sampling_is_ordered_bounded_and_covers_endpoints(
        length in 0usize..20_000,
        limit in 1usize..1_001,
    ) {
        let indices = distributed_indices(length, limit);

        prop_assert_eq!(indices.len(), length.min(limit));
        prop_assert!(indices.iter().all(|index| *index < length));
        prop_assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
        if length > 0 {
            prop_assert_eq!(indices.first(), Some(&0));
        }
        if length > limit && limit > 1 {
            prop_assert_eq!(indices.last(), Some(&(length - 1)));
        }
    }

    #[test]
    fn bson_canonicalization_is_idempotent_and_ignores_document_insertion_order(
        fields in btree_map(field_name(), any::<i64>(), 0..40),
    ) {
        let forward = fields
            .iter()
            .map(|(name, value)| (name.clone(), Bson::Int64(*value)))
            .collect::<Document>();
        let reverse = fields
            .iter()
            .rev()
            .map(|(name, value)| (name.clone(), Bson::Int64(*value)))
            .collect::<Document>();

        let canonical = canonical_bson_document(&forward);
        prop_assert_eq!(&canonical, &canonical_bson_document(&reverse));
        prop_assert_eq!(&canonical, &canonical_bson_document(&canonical));
        prop_assert_eq!(
            mongodb::bson::to_vec(&canonical).unwrap(),
            mongodb::bson::to_vec(&canonical_bson_document(&reverse)).unwrap(),
        );
    }

    #[test]
    fn generated_physical_field_names_are_unique_and_avoid_structural_names(
        sources in vec(field_name(), 0..100),
    ) {
        let fields = sources
            .into_iter()
            .map(|source| (source, NodeEvidence::default()))
            .collect::<BTreeMap<_, _>>();
        let names = physical_field_names(&fields);
        let unique = names.values().collect::<BTreeSet<_>>();

        prop_assert_eq!(names.len(), fields.len());
        prop_assert_eq!(unique.len(), names.len());
        prop_assert!(names.values().all(|name| !name.is_empty()));
        let avoids_structural_names = names.values().all(|name| {
            !matches!(name.as_str(), "_id" | "_parent" | "_pos" | "_value")
        });
        prop_assert!(avoids_structural_names);
    }

    #[test]
    fn generated_identifiers_are_nonempty_bounded_ascii_identifiers(
        source in field_name(),
        uppercase in any::<bool>(),
    ) {
        let generated = identifier(&source, uppercase);

        prop_assert!(!generated.is_empty());
        prop_assert!(generated.len() <= 96);
        prop_assert!(!generated.as_bytes()[0].is_ascii_digit());
        let valid_characters = generated.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_'
        });
        prop_assert!(valid_characters);
        if uppercase {
            prop_assert_eq!(&generated, &generated.to_ascii_uppercase());
        }
    }
}
