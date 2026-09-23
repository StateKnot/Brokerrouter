//! Security contracts shared by the MCP client and server edges.
//!
//! This module deliberately does not expose an MCP endpoint. Wire compatibility is only
//! claimed after the Streamable HTTP/OAuth edges pass the official conformance suite.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "2026-07-28";
pub const SKILLS_EXTENSION: &str = "io.modelcontextprotocol/skills";
pub const MAX_SKILL_RESOURCES: usize = 512;
pub const MAX_SKILL_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 128 * 1024;
const MAX_URI_BYTES: usize = 4096;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("dynamic MCP skills are disabled because approval cannot bind to their content")]
    DynamicSkill,
    #[error("invalid MCP skill URI")]
    InvalidUri,
    #[error("invalid Agent Skills frontmatter")]
    InvalidFrontmatter,
    #[error("invalid MCP skill resource manifest")]
    InvalidManifest,
    #[error("MCP skill exceeds the protocol resource limit")]
    TooManyResources,
    #[error("MCP skill exceeds the protocol byte limit")]
    TooLarge,
    #[error("MCP skill resource is not part of the approved manifest")]
    UnlistedResource,
    #[error("MCP skill resource content does not match its manifest")]
    ContentMismatch,
    #[error("MCP skill frontmatter does not match its advertised entry")]
    FrontmatterMismatch,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillResource {
    pub uri: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum SkillResources {
    Manifest(Vec<SkillResource>),
    Dynamic(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SkillEntry {
    pub uri: String,
    pub frontmatter: Value,
    pub resources: SkillResources,
    /// Untrusted protocol annotations are retained for lossless intermediary use,
    /// but never participate in identity, integrity or approval decisions.
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<BTreeMap<String, Value>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SkillIdentity {
    /// Brokerrouter-assigned immutable identity, never `serverInfo.name`.
    pub server_id: Uuid,
    pub uri: String,
}

#[derive(Clone, Debug)]
pub struct ValidatedSkill {
    identity: SkillIdentity,
    frontmatter: Value,
    resources: BTreeMap<String, SkillResource>,
    approval_fingerprint: String,
    requests_tool_grant: bool,
}

impl SkillEntry {
    /// Validate a static Skills Extension entry and freeze the exact content approval set.
    ///
    /// Dynamic entries are valid on the wire but intentionally declined by Brokerrouter's
    /// production policy: the extension forbids treating persisted approval as covering
    /// future dynamic content.
    pub fn validate_static(self, server_id: Uuid) -> Result<ValidatedSkill, SkillError> {
        let manifest = match self.resources {
            SkillResources::Manifest(resources) => resources,
            SkillResources::Dynamic(value) if value == "dynamic" => {
                return Err(SkillError::DynamicSkill);
            }
            SkillResources::Dynamic(_) => return Err(SkillError::InvalidManifest),
        };
        if manifest.is_empty() {
            return Err(SkillError::InvalidManifest);
        }
        if manifest.len() > MAX_SKILL_RESOURCES {
            return Err(SkillError::TooManyResources);
        }

        let frontmatter = frontmatter(&self.frontmatter)?;
        let name = frontmatter
            .get("name")
            .and_then(Value::as_str)
            .ok_or(SkillError::InvalidFrontmatter)?;
        validate_name(name)?;
        validate_skill_uri(&self.uri, name)?;
        let root = self
            .uri
            .strip_suffix("/SKILL.md")
            .ok_or(SkillError::InvalidUri)?;

        let mut resources = BTreeMap::new();
        let mut total = 0_u64;
        for resource in manifest {
            validate_resource_uri(root, &resource.uri)?;
            validate_digest(&resource.digest)?;
            total = total
                .checked_add(resource.size)
                .ok_or(SkillError::TooLarge)?;
            if total > MAX_SKILL_BYTES {
                return Err(SkillError::TooLarge);
            }
            let uri = resource.uri.clone();
            if resources.insert(uri, resource).is_some() {
                return Err(SkillError::InvalidManifest);
            }
        }
        if !resources.contains_key(&self.uri) {
            return Err(SkillError::InvalidManifest);
        }

        let identity = SkillIdentity {
            server_id,
            uri: self.uri,
        };
        let approval_fingerprint = approval_fingerprint(&identity, resources.values());
        Ok(ValidatedSkill {
            identity,
            requests_tool_grant: frontmatter.contains_key("allowed-tools"),
            frontmatter: self.frontmatter,
            resources,
            approval_fingerprint,
        })
    }
}

impl ValidatedSkill {
    pub fn identity(&self) -> &SkillIdentity {
        &self.identity
    }

    /// Stable hash for persisted approval. It is bound to origin plus the complete
    /// `{uri,digest}` set and changes on file addition, removal or replacement.
    pub fn approval_fingerprint(&self) -> &str {
        &self.approval_fingerprint
    }

    pub fn requests_tool_grant(&self) -> bool {
        self.requests_tool_grant
    }

    pub fn resource_uris(&self) -> impl Iterator<Item = &str> {
        self.resources.keys().map(String::as_str)
    }

    pub fn contains_resource(&self, uri: &str) -> bool {
        self.resources.contains_key(uri)
    }

    pub fn resource_size(&self, uri: &str) -> Option<u64> {
        self.resources.get(uri).map(|resource| resource.size)
    }

    /// Verify bytes lazily at the point of use against the held entry.
    pub fn verify_resource(&self, uri: &str, bytes: &[u8]) -> Result<(), SkillError> {
        let resource = self
            .resources
            .get(uri)
            .ok_or(SkillError::UnlistedResource)?;
        let size = u64::try_from(bytes.len()).map_err(|_| SkillError::ContentMismatch)?;
        if size != resource.size || digest(bytes) != resource.digest {
            return Err(SkillError::ContentMismatch);
        }
        if uri == self.identity.uri {
            let parsed = parse_frontmatter(bytes)?;
            if parsed != self.frontmatter {
                return Err(SkillError::FrontmatterMismatch);
            }
        }
        Ok(())
    }
}

fn frontmatter(value: &Value) -> Result<&serde_json::Map<String, Value>, SkillError> {
    if serde_json::to_vec(value)
        .map_err(|_| SkillError::InvalidFrontmatter)?
        .len()
        > MAX_FRONTMATTER_BYTES
    {
        return Err(SkillError::InvalidFrontmatter);
    }
    let object = value.as_object().ok_or(SkillError::InvalidFrontmatter)?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .ok_or(SkillError::InvalidFrontmatter)?;
    if description.is_empty() || description.chars().count() > 1024 {
        return Err(SkillError::InvalidFrontmatter);
    }
    if let Some(compatibility) = object.get("compatibility") {
        let value = compatibility
            .as_str()
            .ok_or(SkillError::InvalidFrontmatter)?;
        if value.is_empty() || value.chars().count() > 500 {
            return Err(SkillError::InvalidFrontmatter);
        }
    }
    if object
        .get("license")
        .is_some_and(|value| !value.is_string())
    {
        return Err(SkillError::InvalidFrontmatter);
    }
    if let Some(metadata) = object.get("metadata") {
        let values = metadata.as_object().ok_or(SkillError::InvalidFrontmatter)?;
        if !values.values().all(Value::is_string) {
            return Err(SkillError::InvalidFrontmatter);
        }
    }
    if object
        .get("allowed-tools")
        .is_some_and(|value| !value.is_string())
    {
        return Err(SkillError::InvalidFrontmatter);
    }
    Ok(object)
}

fn validate_name(name: &str) -> Result<(), SkillError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || bytes.first() == Some(&b'-')
        || bytes.last() == Some(&b'-')
        || bytes.windows(2).any(|pair| pair == b"--")
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(SkillError::InvalidFrontmatter);
    }
    Ok(())
}

fn validate_skill_uri(uri: &str, name: &str) -> Result<(), SkillError> {
    let parsed = valid_uri(uri)?;
    let segments: Vec<_> = parsed
        .path_segments()
        .ok_or(SkillError::InvalidUri)?
        .collect();
    if segments.last() != Some(&"SKILL.md") {
        return Err(SkillError::InvalidUri);
    }
    let parent = if segments.len() > 1 {
        segments[segments.len() - 2]
    } else {
        parsed.host_str().ok_or(SkillError::InvalidUri)?
    };
    if parent != name {
        return Err(SkillError::InvalidUri);
    }
    Ok(())
}

fn validate_resource_uri(root: &str, uri: &str) -> Result<(), SkillError> {
    let root = valid_uri(root)?;
    let resource = valid_uri(uri)?;
    let root_path = root.path().trim_end_matches('/');
    let expected_path = format!("{root_path}/");
    if resource.scheme() != root.scheme()
        || resource.host_str() != root.host_str()
        || resource.port_or_known_default() != root.port_or_known_default()
        || !resource.path().starts_with(&expected_path)
        || uri.contains('\\')
        || uri.to_ascii_lowercase().contains("%2f")
        || uri.to_ascii_lowercase().contains("%5c")
        || uri.split('/').any(|part| {
            let lower = part.to_ascii_lowercase();
            matches!(
                lower.as_str(),
                "." | ".." | "%2e" | "%2e%2e" | ".%2e" | "%2e."
            )
        })
    {
        return Err(SkillError::InvalidManifest);
    }
    Ok(())
}

fn valid_uri(uri: &str) -> Result<Url, SkillError> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES {
        return Err(SkillError::InvalidUri);
    }
    let parsed = Url::parse(uri).map_err(|_| SkillError::InvalidUri)?;
    if parsed.scheme().is_empty()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(SkillError::InvalidUri);
    }
    Ok(parsed)
}

fn validate_digest(value: &str) -> Result<(), SkillError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(SkillError::InvalidManifest);
    };
    if hex.len() != 64
        || !hex
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(SkillError::InvalidManifest);
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in hash {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn approval_fingerprint<'a>(
    identity: &SkillIdentity,
    resources: impl Iterator<Item = &'a SkillResource>,
) -> String {
    fn add(hasher: &mut Sha256, value: &[u8]) {
        hasher.update(
            u64::try_from(value.len())
                .expect("bounded MCP value")
                .to_be_bytes(),
        );
        hasher.update(value);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"brokerrouter-mcp-skill-approval-v1");
    hasher.update(identity.server_id.as_bytes());
    add(&mut hasher, identity.uri.as_bytes());
    for resource in resources {
        add(&mut hasher, resource.uri.as_bytes());
        add(&mut hasher, resource.digest.as_bytes());
    }
    let result = hasher.finalize();
    let mut value = String::with_capacity(64);
    for byte in result {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn parse_frontmatter(bytes: &[u8]) -> Result<Value, SkillError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SkillError::FrontmatterMismatch)?;
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().ok_or(SkillError::FrontmatterMismatch)?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(SkillError::FrontmatterMismatch);
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        if yaml.len().saturating_add(line.len()) > MAX_FRONTMATTER_BYTES {
            return Err(SkillError::FrontmatterMismatch);
        }
        yaml.push_str(line);
    }
    if !closed || yaml.is_empty() {
        return Err(SkillError::FrontmatterMismatch);
    }
    let parsed: Value =
        serde_yaml_ng::from_str(&yaml).map_err(|_| SkillError::FrontmatterMismatch)?;
    frontmatter(&parsed).map_err(|_| SkillError::FrontmatterMismatch)?;
    Ok(parsed)
}

/// Detect ambiguous name-only registries before a host exposes Skills to a model.
pub fn colliding_names<'a>(
    skills: impl IntoIterator<Item = &'a ValidatedSkill>,
) -> BTreeSet<String> {
    let mut first = BTreeMap::<String, &SkillIdentity>::new();
    let mut collisions = BTreeSet::new();
    for skill in skills {
        let name = skill
            .frontmatter
            .get("name")
            .and_then(Value::as_str)
            .expect("validated skill name");
        if first.insert(name.to_owned(), &skill.identity).is_some() {
            collisions.insert(name.to_owned());
        }
    }
    collisions
}
