use executor::Uuid7;

use crate::generate_fun_slug_from_uuid;

#[test]
fn generated_conversation_slug_has_expected_shape() {
    let uuid = "019da9fe-67ea-7b62-9d70-3e6070b08878"
        .parse::<Uuid7>()
        .expect("uuid should parse");

    let slug = generate_fun_slug_from_uuid(uuid);

    assert_eq!(slug, "aster-lantern-8878");
}

#[test]
fn generated_conversation_slug_is_lowercase_and_hyphenated() {
    let uuid = "019da9a0-6f27-741f-9bd2-731472dfea75"
        .parse::<Uuid7>()
        .expect("uuid should parse");

    let slug = generate_fun_slug_from_uuid(uuid);

    assert!(
        slug.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    );
    assert_eq!(slug.split('-').count(), 3);
}
