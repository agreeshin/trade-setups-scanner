use serde::Deserialize;
use setups_api::SessionAuth;

#[derive(Deserialize)]
struct Fixture {
    auth: AuthSection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthSection {
    sign_cases: Vec<SignCase>,
}

#[derive(Deserialize)]
struct SignCase {
    payload: String,
    hmac: String,
}

#[test]
fn hmac_signatures_match_typescript() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../fixtures/parity.json")).expect("fixture");
    assert_eq!(fixture.auth.sign_cases.len(), 2);
    let auth = SessionAuth::with_signing_key(&[], [7u8; 32]);
    for case in &fixture.auth.sign_cases {
        assert_eq!(
            auth.sign(&case.payload),
            case.hmac,
            "session signature must match node crypto HMAC-SHA256"
        );
    }
}
