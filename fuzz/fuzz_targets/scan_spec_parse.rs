#![no_main]

use libfuzzer_sys::fuzz_target;
use mongodb_vs::wire::MongoScanSpec;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(spec) = MongoScanSpec::from_json(input) else {
        return;
    };

    let encoded = spec
        .to_json()
        .expect("a parsed scan specification must serialize");
    let reparsed = MongoScanSpec::from_json(&encoded)
        .expect("a parsed and serialized scan specification must remain valid");
    assert_eq!(reparsed, spec);
});
