use mongodb::bson::Bson;
use proptest::collection::vec;
use proptest::prelude::*;

use super::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn varchar_conversion_uses_character_count_not_utf8_bytes(
        characters in vec(any::<char>(), 0..160),
        size in 1u32..128,
    ) {
        let value = characters.into_iter().collect::<String>();
        let expected = value.chars().count() <= size as usize;
        let result = string_value(value.clone(), &SqlType::Varchar { size }, "value");

        prop_assert_eq!(result.is_ok(), expected);
        if let Ok(Value::String(converted)) = result {
            prop_assert_eq!(converted, value);
        }
    }

    #[test]
    fn integer_conversion_accepts_exactly_the_declared_decimal_width(
        value in any::<i128>(),
        precision in 1u32..=36,
    ) {
        let digits = value.unsigned_abs().to_string().len() as u32;
        let result = integer_value(
            value,
            &SqlType::Decimal { precision, scale: 0 },
            "value",
        );

        prop_assert_eq!(result.is_ok(), digits <= precision);
        if let Ok(Value::Numeric(converted)) = result {
            prop_assert_eq!(converted.unscaled, value);
            prop_assert_eq!(converted.scale, 0);
        }
    }

    #[test]
    fn stable_row_ids_are_repeatable_lowercase_sha256(
        root in vec(any::<char>(), 0..80).prop_map(|chars| chars.into_iter().collect::<String>()),
        ordinals in vec(any::<u64>(), 0..12),
    ) {
        let path = [PathSegment {
            name: "items".into(),
            kind: PathKind::Array,
            direct: false,
        }];
        let first = stable_id(&Bson::String(root.clone()), &path, &ordinals).unwrap();
        let second = stable_id(&Bson::String(root), &path, &ordinals).unwrap();

        prop_assert_eq!(&first, &second);
        prop_assert_eq!(first.len(), 64);
        prop_assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    }
}
