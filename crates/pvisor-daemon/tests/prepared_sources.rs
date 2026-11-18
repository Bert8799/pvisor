use pvisor_daemon::api::{PreparedImage, PreparedImageSource};
use serde_json::{Value, json};

fn directory() -> Value {
    json!({
        "source": "directory_template", "rootfs": "/private/rootfs",
        "entrypoint": ["/guest/bootstrap"], "cmd": ["/bin/sleep", "infinity"],
        "template": {
            "catalog": "/private/templates", "name": "agent",
            "image_sha256": "a".repeat(64)
        }
    })
}

fn pooled() -> Value {
    json!({
        "rootfs": "/private/store", "entrypoint": ["/guest/bootstrap"],
        "pooled_image": {
            "reference": "fixture:latest", "id": format!("sha256:{}", "1".repeat(64)),
            "digest": format!("sha256:{}", "2".repeat(64)), "platform": "linux/amd64",
            "cache": {
                "location": "/private/store/images/pool-v2",
                "handle": format!("pvisor-v2:{}:linux-amd64:{}", "3".repeat(64), "4".repeat(64))
            },
            "env": {}, "entrypoint": [], "cmd": [], "cwd": "/", "user": "",
            "stop_signal": 15, "volumes": [], "created_ms": 0
        }
    })
}

#[test]
fn pooled_sources_preserve_the_existing_image_contract_and_wire_format() {
    let image: PreparedImage = serde_json::from_value(pooled()).unwrap();
    let existing_wire = serde_json::to_value(image).unwrap();
    let source: PreparedImageSource = serde_json::from_value(existing_wire.clone()).unwrap();
    assert!(matches!(source, PreparedImageSource::Pooled(_)));
    assert_eq!(serde_json::to_value(source).unwrap(), existing_wire);
    let mut missing_binding = pooled();
    missing_binding
        .as_object_mut()
        .unwrap()
        .remove("pooled_image");
    assert!(serde_json::from_value::<PreparedImage>(missing_binding.clone()).is_err());
    assert!(serde_json::from_value::<PreparedImageSource>(missing_binding).is_err());
}

#[test]
fn directory_sources_require_the_tag_and_template_without_a_pool_binding() {
    let wire = directory();
    let source: PreparedImageSource = serde_json::from_value(wire.clone()).unwrap();
    assert!(matches!(source, PreparedImageSource::DirectoryTemplate(_)));
    assert_eq!(serde_json::to_value(source).unwrap(), wire);
    assert!(serde_json::from_value::<PreparedImage>(wire.clone()).is_err());
    for case in [
        "missing_tag",
        "wrong_tag",
        "missing_template",
        "null_template",
        "unknown_field",
        "mixed_pool",
    ] {
        let mut invalid = wire.clone();
        match case {
            "missing_tag" => {
                invalid.as_object_mut().unwrap().remove("source");
            }
            "wrong_tag" => invalid["source"] = "pooled".into(),
            "missing_template" => {
                invalid.as_object_mut().unwrap().remove("template");
            }
            "null_template" => invalid["template"] = Value::Null,
            "unknown_field" => invalid["unexpected"] = true.into(),
            "mixed_pool" => invalid["pooled_image"] = pooled()["pooled_image"].clone(),
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<PreparedImageSource>(invalid).is_err(),
            "{case}"
        );
    }
}

#[test]
fn source_selection_survives_persistence_with_the_same_template_identity() {
    let mut wire = directory();
    wire["cwd"] = "/workspace/project".into();
    wire["env"] = json!({"PATH": "/bin", "LANG": "C.UTF-8"});
    wire["native_guest"] = true.into();
    wire["entrypoint"] = json!([]);
    let source: PreparedImageSource = serde_json::from_value(wire).unwrap();
    let bytes = serde_json::to_vec(&source).unwrap();
    let restored: PreparedImageSource = serde_json::from_slice(&bytes).unwrap();
    let PreparedImageSource::DirectoryTemplate(restored) = restored else {
        panic!("directory source changed")
    };
    assert_eq!(
        restored.template.catalog.to_str(),
        Some("/private/templates")
    );
    assert_eq!(restored.template.name, "agent");
    assert_eq!(restored.template.image_sha256, "a".repeat(64));
    assert_eq!(
        restored.cwd.as_deref(),
        Some(std::path::Path::new("/workspace/project"))
    );
    assert_eq!(restored.env["PATH"], "/bin");
    assert!(restored.native_guest);
}
