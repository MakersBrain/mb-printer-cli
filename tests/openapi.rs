// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::HashSet;

fn local_refs<'a>(value: &'a serde_yaml::Value, output: &mut Vec<&'a str>) {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if key.as_str() == Some("$ref")
                    && let Some(reference) = value.as_str()
                {
                    output.push(reference);
                }
                local_refs(value, output);
            }
        }
        serde_yaml::Value::Sequence(sequence) => {
            for value in sequence {
                local_refs(value, output);
            }
        }
        _ => {}
    }
}

fn resolves(document: &serde_yaml::Value, reference: &str) -> bool {
    reference.strip_prefix("#/").is_some_and(|pointer| {
        pointer
            .split('/')
            .try_fold(document, |value, segment| value.get(segment))
            .is_some()
    })
}

#[test]
fn openapi_contract_parses_and_has_unique_operations_and_local_refs() {
    let source = include_str!("../docs/openapi.yaml");
    let document: serde_yaml::Value =
        serde_yaml::from_str(source).expect("OpenAPI YAML must parse");
    assert_eq!(document["openapi"].as_str(), Some("3.1.0"));
    let paths = document["paths"].as_mapping().expect("paths object");
    let mut operations = HashSet::new();
    for methods in paths.values().filter_map(serde_yaml::Value::as_mapping) {
        for operation in methods.values().filter_map(serde_yaml::Value::as_mapping) {
            if let Some(id) = operation
                .get(serde_yaml::Value::String("operationId".into()))
                .and_then(serde_yaml::Value::as_str)
            {
                assert!(operations.insert(id), "duplicate operationId {id}");
            }
        }
    }
    for required in [
        "pair",
        "currentGrant",
        "rotateCurrentGrant",
        "revokeCurrentGrant",
        "previewDocument",
        "submitJob",
        "jobEvents",
        "extractLaposte",
    ] {
        assert!(
            operations.contains(required),
            "missing operation {required}"
        );
    }
    let mut references = Vec::new();
    local_refs(&document, &mut references);
    for reference in references {
        assert!(
            resolves(&document, reference),
            "unresolved local reference {reference}"
        );
    }
}
