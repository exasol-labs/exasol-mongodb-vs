#![no_main]

use libfuzzer_sys::fuzz_target;
use mongodb_vs::model::ExplicitManifest;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(manifest) = ExplicitManifest::parse(input) else {
        return;
    };

    let encoded = serde_json::to_string(&manifest).expect("a parsed manifest must serialize");
    let reparsed = ExplicitManifest::parse(&encoded)
        .expect("a parsed and serialized manifest must remain valid");
    assert_eq!(reparsed, manifest);
});
