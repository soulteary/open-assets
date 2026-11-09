use open_assets::common;
use serde_json::Value;

#[test]
fn python_generated_digest_fixtures() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/python-json.json")).unwrap();
    for fixture in fixtures {
        let bytes = common::python_json(&fixture["value"], true).unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            fixture["expected"].as_str().unwrap(),
            "value={}",
            fixture["value"]
        );
        assert_eq!(
            common::digest(&fixture["value"]).unwrap(),
            fixture["digest"].as_str().unwrap()
        );
    }
}

#[test]
fn original_yaml_configuration_revision_is_byte_exact() {
    use sha2::{Digest, Sha256};
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../config/repositories.example.yaml");
    let bytes = std::fs::read(&file).unwrap();
    let config = common::load(Some(&file)).unwrap();
    assert_eq!(config["revision"], hex::encode(Sha256::digest(bytes)));
    assert_eq!(config["publication"]["reference_releases"], false);
}

#[test]
fn python_generated_restoration_validation_fixtures() {
    use open_assets::info::reports::{clean_check, clean_closure, clean_restore};
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/python-reports.json")).unwrap();
    for (index, fixture) in fixtures.into_iter().enumerate() {
        let sha = "a".repeat(40);
        let result = match fixture["kind"].as_str().unwrap() {
            "restore" => clean_restore(&fixture["value"], "assets", &sha, &"b".repeat(64)),
            "check" => clean_check(&fixture["value"], "assets", &sha),
            "closure" => clean_closure(
                &fixture["intent"],
                Some(&fixture["value"]),
                "assets",
                &"c".repeat(32),
            ),
            _ => panic!("invalid fixture"),
        };
        assert_eq!(
            result.is_ok(),
            fixture["accepted"].as_bool().unwrap(),
            "fixture {index}: {}",
            fixture
        );
        if let Ok(actual) = result {
            assert_eq!(actual, fixture["expected"], "fixture {index}");
        }
    }
}
