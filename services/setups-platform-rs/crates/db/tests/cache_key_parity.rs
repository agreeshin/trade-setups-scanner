use serde::Deserialize;
use setups_db::{CacheQuery, canonical_cache_key};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    cache_keys: CacheKeys,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheKeys {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    query: CacheQuery,
    key: String,
}

#[test]
fn canonical_cache_key_parity() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../fixtures/parity.json")).expect("fixture");
    assert!(!fixture.cache_keys.cases.is_empty());
    for case in &fixture.cache_keys.cases {
        assert_eq!(
            canonical_cache_key(&case.query),
            case.key,
            "cache key must match TypeScript canonicalCacheKey"
        );
    }
}
