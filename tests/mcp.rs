use brokerrouter::mcp::{
    MAX_FRONTMATTER_BYTES, MAX_SKILL_BYTES, MAX_SKILL_RESOURCES, SkillEntry, SkillError,
    SkillResource, SkillResources, colliding_names,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn skill(server: Uuid, name: &str, body: &[u8]) -> brokerrouter::mcp::ValidatedSkill {
    let uri = format!("skill://acme/{name}/SKILL.md");
    SkillEntry {
        uri: uri.clone(),
        frontmatter: json!({"name":name,"description":"A governed skill"}),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri,
            digest: digest(body),
            size: body.len() as u64,
        }]),
        meta: None,
    }
    .validate_static(server)
    .unwrap()
}

#[test]
fn validates_origin_bound_manifest_and_lazy_bytes() {
    let server = Uuid::new_v4();
    let content =
        b"---\nname: code-review\ndescription: A governed skill\n---\nReview carefully.\n";
    let skill = skill(server, "code-review", content);
    assert_eq!(skill.identity().server_id, server);
    assert_eq!(skill.identity().uri, "skill://acme/code-review/SKILL.md");
    assert!(!skill.requests_tool_grant());
    assert_eq!(skill.resource_uris().count(), 1);
    skill
        .verify_resource("skill://acme/code-review/SKILL.md", content)
        .unwrap();
    assert_eq!(skill.approval_fingerprint().len(), 64);
}

#[test]
fn rejects_dynamic_unlisted_tampered_and_frontmatter_mismatch() {
    let server = Uuid::new_v4();
    let dynamic = SkillEntry {
        uri: "skill://dynamic/SKILL.md".into(),
        frontmatter: json!({"name":"dynamic","description":"Generated"}),
        resources: SkillResources::Dynamic("dynamic".into()),
        meta: None,
    };
    assert_eq!(
        dynamic.validate_static(server).unwrap_err(),
        SkillError::DynamicSkill
    );

    let content = b"---\nname: safe\ndescription: A governed skill\n---\nBody\n";
    let skill = skill(server, "safe", content);
    assert_eq!(
        skill.verify_resource("skill://acme/safe/other.md", b"x"),
        Err(SkillError::UnlistedResource)
    );
    assert_eq!(
        skill.verify_resource("skill://acme/safe/SKILL.md", b"tampered"),
        Err(SkillError::ContentMismatch)
    );

    let wrong = b"---\nname: safe\ndescription: Different\n---\nBody\n";
    let uri = "skill://acme/safe/SKILL.md".to_string();
    let entry = SkillEntry {
        uri: uri.clone(),
        frontmatter: json!({"name":"safe","description":"A governed skill"}),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri,
            digest: digest(wrong),
            size: wrong.len() as u64,
        }]),
        meta: None,
    }
    .validate_static(server)
    .unwrap();
    assert_eq!(
        entry.verify_resource("skill://acme/safe/SKILL.md", wrong),
        Err(SkillError::FrontmatterMismatch)
    );
}

#[test]
fn approval_changes_with_origin_or_complete_resource_set() {
    let content = b"---\nname: deploy\ndescription: A governed skill\n---\nBody\n";
    let first = skill(Uuid::new_v4(), "deploy", content);
    let second = skill(Uuid::new_v4(), "deploy", content);
    assert_ne!(first.approval_fingerprint(), second.approval_fingerprint());

    let server = Uuid::new_v4();
    let uri = "skill://acme/deploy/SKILL.md".to_string();
    let mut resources = vec![SkillResource {
        uri: uri.clone(),
        digest: digest(content),
        size: content.len() as u64,
    }];
    let base = SkillEntry {
        uri: uri.clone(),
        frontmatter: json!({"name":"deploy","description":"A governed skill"}),
        resources: SkillResources::Manifest(resources.clone()),
        meta: None,
    }
    .validate_static(server)
    .unwrap();
    resources.push(SkillResource {
        uri: "skill://acme/deploy/references/policy.md".into(),
        digest: digest(b"policy"),
        size: 6,
    });
    let changed = SkillEntry {
        uri,
        frontmatter: json!({"name":"deploy","description":"A governed skill"}),
        resources: SkillResources::Manifest(resources),
        meta: None,
    }
    .validate_static(server)
    .unwrap();
    assert_ne!(base.approval_fingerprint(), changed.approval_fingerprint());
}

#[test]
fn enforces_agent_skill_shape_and_extension_limits() {
    let server = Uuid::new_v4();
    let invalid_name = SkillEntry {
        uri: "skill://Bad/SKILL.md".into(),
        frontmatter: json!({"name":"Bad","description":"No"}),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri: "skill://Bad/SKILL.md".into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size: 1,
        }]),
        meta: None,
    };
    assert_eq!(
        invalid_name.validate_static(server).unwrap_err(),
        SkillError::InvalidFrontmatter
    );

    let too_many = (0..=MAX_SKILL_RESOURCES)
        .map(|index| SkillResource {
            uri: if index == 0 {
                "skill://many/SKILL.md".into()
            } else {
                format!("skill://many/{index}.md")
            },
            digest: format!("sha256:{}", "0".repeat(64)),
            size: 0,
        })
        .collect();
    let entry = SkillEntry {
        uri: "skill://many/SKILL.md".into(),
        frontmatter: json!({"name":"many","description":"Too many"}),
        resources: SkillResources::Manifest(too_many),
        meta: None,
    };
    assert_eq!(
        entry.validate_static(server).unwrap_err(),
        SkillError::TooManyResources
    );

    let entry = SkillEntry {
        uri: "skill://large/SKILL.md".into(),
        frontmatter: json!({"name":"large","description":"Too large"}),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri: "skill://large/SKILL.md".into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size: MAX_SKILL_BYTES + 1,
        }]),
        meta: None,
    };
    assert_eq!(
        entry.validate_static(server).unwrap_err(),
        SkillError::TooLarge
    );

    let entry = SkillEntry {
        uri: "skill://metadata/SKILL.md".into(),
        frontmatter: json!({
            "name":"metadata",
            "description":"Bounded",
            "future": "x".repeat(MAX_FRONTMATTER_BYTES)
        }),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri: "skill://metadata/SKILL.md".into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size: 1,
        }]),
        meta: None,
    };
    assert_eq!(
        entry.validate_static(server).unwrap_err(),
        SkillError::InvalidFrontmatter
    );
}

#[test]
fn same_name_is_never_silently_merged_across_origins() {
    let content = b"---\nname: audit\ndescription: A governed skill\n---\nBody\n";
    let first = skill(Uuid::new_v4(), "audit", content);
    let second = skill(Uuid::new_v4(), "audit", content);
    assert_eq!(
        colliding_names([&first, &second]),
        ["audit".to_string()].into_iter().collect()
    );
}

#[test]
fn accepts_domain_specific_uris_and_rejects_ambiguous_paths() {
    let server = Uuid::new_v4();
    let content =
        b"---\nname: refunds\ndescription: A governed skill\nlicense: Apache-2.0\n---\nBody\n";
    let uri = "file:///srv/skills/refunds/SKILL.md".to_string();
    let entry = SkillEntry {
        uri: uri.clone(),
        frontmatter: json!({
            "name":"refunds",
            "description":"A governed skill",
            "license":"Apache-2.0"
        }),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri,
            digest: digest(content),
            size: content.len() as u64,
        }]),
        meta: Some(
            [(
                "com.brokerrouter/provenance".into(),
                json!({"label":"docs"}),
            )]
            .into_iter()
            .collect(),
        ),
    }
    .validate_static(server)
    .unwrap();
    entry
        .verify_resource("file:///srv/skills/refunds/SKILL.md", content)
        .unwrap();

    for resource_uri in [
        "skill://acme/safe/../outside.md",
        "skill://acme/safe/%2e%2e/outside.md",
        "skill://acme/safe/%2Foutside.md",
        "skill://other/safe/reference.md",
    ] {
        let skill_uri = "skill://acme/safe/SKILL.md".to_string();
        let invalid = SkillEntry {
            uri: skill_uri.clone(),
            frontmatter: json!({"name":"safe","description":"A governed skill"}),
            resources: SkillResources::Manifest(vec![
                SkillResource {
                    uri: skill_uri,
                    digest: format!("sha256:{}", "0".repeat(64)),
                    size: 1,
                },
                SkillResource {
                    uri: resource_uri.into(),
                    digest: format!("sha256:{}", "0".repeat(64)),
                    size: 1,
                },
            ]),
            meta: None,
        };
        assert_eq!(
            invalid.validate_static(server).unwrap_err(),
            SkillError::InvalidManifest
        );
    }
}

#[test]
fn untrusted_annotations_never_change_approval_or_grant_tools() {
    let server = Uuid::new_v4();
    let content = b"---\nname: remote-code\ndescription: Requests local execution\nallowed-tools: Bash\n---\nBody\n";
    let uri = "skill://remote-code/SKILL.md".to_string();
    let base = SkillEntry {
        uri: uri.clone(),
        frontmatter: json!({
            "name":"remote-code",
            "description":"Requests local execution",
            "allowed-tools":"Bash"
        }),
        resources: SkillResources::Manifest(vec![SkillResource {
            uri,
            digest: digest(content),
            size: content.len() as u64,
        }]),
        meta: None,
    };
    let mut annotated = base.clone();
    annotated.meta = Some(
        [("com.example/verified".into(), json!(true))]
            .into_iter()
            .collect(),
    );
    let base = base.validate_static(server).unwrap();
    let annotated = annotated.validate_static(server).unwrap();
    assert!(base.requests_tool_grant());
    assert_eq!(
        base.approval_fingerprint(),
        annotated.approval_fingerprint()
    );
}
