//! Startup-only, purpose-scoped additions to the platform HTTPS trust roots.
use crate::error::{Error, Result};
use base64::{Engine, prelude::BASE64_STANDARD};
use std::{fs::File, io::Read, path::Path};

const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
const MAX_CERTIFICATES: usize = 128;

/// Only the caller's deployment-owned variable selects additional roots; no
/// fallback to another purpose or request-supplied path is allowed. The client's
/// existing platform trust configuration is left intact.
pub(crate) fn with_ca_file(
    mut builder: reqwest::ClientBuilder,
    variable: &str,
) -> Result<reqwest::ClientBuilder> {
    let path = match std::env::var(variable) {
        Ok(path) if !path.is_empty() => path,
        Err(std::env::VarError::NotPresent) => return Ok(builder),
        _ => return Err(Error::Configuration),
    };
    for certificate in read_certificates(Path::new(&path))? {
        builder = builder.add_root_certificate(certificate);
    }
    // reqwest validates DER when the caller builds the client. Keep that error
    // fatal at startup; never replace a failed bundle with platform roots only.
    Ok(builder)
}

fn read_certificates(path: &Path) -> Result<Vec<reqwest::Certificate>> {
    let metadata = std::fs::metadata(path).map_err(|_| Error::Configuration)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_BUNDLE_BYTES as u64 {
        return Err(Error::Configuration);
    }
    let file = File::open(path).map_err(|_| Error::Configuration)?;
    let metadata = file.metadata().map_err(|_| Error::Configuration)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_BUNDLE_BYTES as u64 {
        return Err(Error::Configuration);
    }
    let mut pem = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_BUNDLE_BYTES + 1) as u64)
        .read_to_end(&mut pem)
        .map_err(|_| Error::Configuration)?;
    if pem.is_empty() || pem.len() > MAX_BUNDLE_BYTES {
        return Err(Error::Configuration);
    }
    certificates(&pem)
}

fn certificates(pem: &[u8]) -> Result<Vec<reqwest::Certificate>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut rest = std::str::from_utf8(pem).map_err(|_| Error::Configuration)?;
    let mut roots = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() {
            return if roots.is_empty() {
                Err(Error::Configuration)
            } else {
                Ok(roots)
            };
        }
        if roots.len() == MAX_CERTIFICATES {
            return Err(Error::Configuration);
        }
        let body = rest.strip_prefix(BEGIN).ok_or(Error::Configuration)?;
        let end = body.find(END).ok_or(Error::Configuration)?;
        let encoded: Vec<u8> = body[..end]
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let der = BASE64_STANDARD
            .decode(encoded)
            .map_err(|_| Error::Configuration)?;
        if der.is_empty() {
            return Err(Error::Configuration);
        }
        roots.push(reqwest::Certificate::from_der(&der).map_err(|_| Error::Configuration)?);
        rest = &body[end + END.len()..];
    }
}

#[cfg(test)]
mod tests;
