//! Single-flight security workload. Only this process holds a scanner credential.
//! Durable claim/evidence precede external actions; failed uploads never rerun generation.
use crate::{
    output_scans::{Content, Report, Technical},
    scanner_engine::{self, Engine, Manifest},
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const CHUNK: u64 = 4 * 1024 * 1024;
const JSON_LIMIT: u64 = 64 * 1024;
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api_origin: String,
    pub docker_binary: PathBuf,
    pub docker_socket: PathBuf,
    pub work_dir: PathBuf,
    pub manifest: Manifest,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Active {
    claim_id: Uuid,
    attempt_id: Option<Uuid>,
}
#[derive(Deserialize)]
struct Assignment {
    attempt_id: Uuid,
    scan_id: Uuid,
    job_id: Uuid,
    asset_id: Uuid,
    sha256: String,
    byte_length: u64,
    chunk_count: u64,
    profile_version: i32,
    engine_revision: String,
    state: String,
}
pub struct Worker {
    config: Config,
    engine: Engine,
    client: Client,
    token: String,
    _lock: File,
}
impl Worker {
    pub fn new(config: Config, token: String) -> Result<Self> {
        config.manifest.validate()?;
        ensure!(
            token.starts_with("sc_br_")
                && token.len() == 70
                && scanner_engine::hash_text(&token[6..]),
            "invalid scanner credential"
        );
        let url = url::Url::parse(&config.api_origin)?;
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && [Some("127.0.0.1"), Some("[::1]")].contains(&url.host_str())),
            "scanner API requires TLS except numeric loopback fixtures"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "scanner API must be an origin without credentials"
        );
        ensure!(
            config.docker_binary.is_absolute()
                && config.docker_socket.is_absolute()
                && config.work_dir.is_absolute(),
            "scanner paths must be absolute"
        );
        ensure!(
            fs::metadata(&config.docker_socket)?.file_type().is_socket(),
            "Docker socket must be local Unix socket"
        );
        let root = config.work_dir.canonicalize()?;
        ensure!(
            root == config.work_dir && !root.to_string_lossy().contains([',', '\n', '\r']),
            "work directory must be canonical"
        );
        ensure!(
            fs::metadata(&root)?.permissions().mode() & 0o077 == 0,
            "work directory must be private (0700)"
        );
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(root.join("worker.lock"))?;
        lock.try_lock().context("work directory already in use")?;
        // Docker's client config can inject proxy credentials into containers even with env_clear.
        // Never read a personal/default config; keep an explicit, private, empty one.
        let docker_config = root.join("docker-config");
        if !docker_config.exists() {
            fs::create_dir(&docker_config)?;
            fs::set_permissions(&docker_config, fs::Permissions::from_mode(0o700))?;
        }
        ensure!(
            fs::symlink_metadata(&docker_config)?.is_dir()
                && docker_config.canonicalize()? == docker_config,
            "invalid isolated Docker config directory"
        );
        let config_file = docker_config.join("config.json");
        if config_file.exists() {
            ensure!(
                read_limited(&config_file, JSON_LIMIT)? == b"{}",
                "isolated Docker config must be empty"
            );
        } else {
            persist_bytes(&config_file, b"{}")?;
        }
        let engine = Engine {
            docker_binary: config.docker_binary.clone(),
            docker_socket: config.docker_socket.clone(),
            docker_config,
            manifest: config.manifest.clone(),
        };
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(45))
            .build()?;
        let worker = Self {
            config,
            engine,
            client,
            token,
            _lock: lock,
        };
        worker.ensure_evidence_capacity()?;
        Ok(worker)
    }
    pub async fn preflight(&self) -> Result<()> {
        self.engine.preflight().await
    }
    pub async fn abort_current(&self) -> Result<()> {
        if self.path("active.json").exists() {
            let active: Active = read_json(&self.path("active.json"))?;
            if let Some(id) = active.attempt_id {
                self.engine.cleanup(id).await?;
            }
        }
        self.clear_input()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.config.work_dir.join(name)
    }
    /// One assignment (or one retry). Transient failures retain its durable claim and receipt.
    pub async fn once(&self) -> Result<Value> {
        let result = self.once_inner().await;
        if result.is_err() {
            self.clear_input()?;
        }
        result
    }
    async fn once_inner(&self) -> Result<Value> {
        self.config.manifest.validate()?;
        self.ensure_evidence_capacity()?;
        let mut active: Active = if self.path("active.json").exists() {
            read_json(&self.path("active.json"))?
        } else {
            let a = Active {
                claim_id: Uuid::new_v4(),
                attempt_id: None,
            };
            persist(&self.path("active.json"), &a)?;
            a
        };
        if let Some(id) = active.attempt_id {
            self.engine.cleanup(id).await?;
        }
        let (status, v) = self
            .json(
                Method::POST,
                "/v1/output-scans/claim",
                json!({"claim_id":active.claim_id}),
            )
            .await?;
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::CONFLICT | StatusCode::GONE
        ) {
            self.clear()?;
            return Ok(json!({"state":"attempt_closed"}));
        }
        ensure!(
            status == StatusCode::OK,
            "scanner claim rejected (HTTP {})",
            status.as_u16()
        );
        if v["state"] == "no_assignment" {
            return Ok(json!({"state":"idle"}));
        }
        let a: Assignment = serde_json::from_value(v).context("invalid scan assignment")?;
        ensure!(
            a.engine_revision == self.config.manifest.revision()?
                && scanner_engine::hash_text(&a.sha256)
                && (1..=scanner_engine::MAX_BYTES).contains(&a.byte_length)
                && a.chunk_count == a.byte_length.div_ceil(CHUNK)
                && a.profile_version > 0,
            "assignment differs from pinned detector contract"
        );
        if a.state == "complete" {
            self.clear()?;
            return Ok(json!({"state":"complete","attempt_id":a.attempt_id,"recovered":true}));
        }
        ensure!(a.state == "scanning", "assignment not runnable");
        if active.attempt_id.is_some_and(|id| id != a.attempt_id) {
            self.clear_input()?;
        }
        active.attempt_id = Some(a.attempt_id);
        persist(&self.path("active.json"), &active)?;
        let receipt_path = self.path(&format!("receipt-{}.json", a.attempt_id));
        let report: Report = if receipt_path.exists() {
            let report: Report = read_json(&receipt_path)?;
            ensure!(
                report.sha256 == a.sha256
                    && report.byte_length == a.byte_length as i64
                    && report.engine_revision == a.engine_revision
                    && report.profile_version == a.profile_version,
                "durable receipt conflicts with assignment"
            );
            let evidence = self.path(&format!("evidence-{}.json", report.evidence_sha256));
            ensure!(
                scanner_engine::hex_hash(&read_limited(&evidence, JSON_LIMIT)?)
                    == report.evidence_sha256,
                "durable evidence missing or corrupted"
            );
            report
        } else {
            self.download(&a).await?;
            let mut steps = Vec::new();
            for step in ["antivirus", "probe", "decode"] {
                self.renew(&a).await?;
                let result = self
                    .engine
                    .step(a.attempt_id, &self.path("input"), step)
                    .await?;
                let passed = result["outcome"] == "passed";
                steps.push(result);
                if !passed {
                    break;
                }
            }
            let finding = scanner_engine::finding(&steps)?;
            let evidence = json!({"contract":scanner_engine::CONTRACT,"manifest":self.config.manifest,"engine_revision":a.engine_revision,"attempt_id":a.attempt_id,"scan_id":a.scan_id,"job_id":a.job_id,"asset_id":a.asset_id,"sha256":a.sha256,"byte_length":a.byte_length,"steps":finding.evidence,"content":"unknown","semantic_detector":"not_run"});
            let evidence_bytes = serde_json::to_vec(&evidence)?;
            let hash = scanner_engine::hex_hash(&evidence_bytes);
            persist_bytes(
                &self.path(&format!("evidence-{hash}.json")),
                &evidence_bytes,
            )?;
            let report = Report {
                sha256: a.sha256.clone(),
                byte_length: a.byte_length as i64,
                profile_version: a.profile_version,
                engine_revision: a.engine_revision.clone(),
                media_type: if finding.technical == Technical::Clean {
                    Some("video/mp4".into())
                } else {
                    None
                },
                technical: finding.technical,
                content: Content::Unknown,
                reason_codes: vec![finding.reason_code.into()],
                evidence_sha256: hash,
                scanned_at: Utc::now(),
            };
            persist(&receipt_path, &report)?;
            // The proof has no media bytes. Delete the only plaintext copy before publishing it.
            self.clear_input()?;
            report
        };
        self.renew(&a).await?;
        let (status, v) = self
            .json(
                Method::POST,
                &format!("/v1/output-scans/attempts/{}/report", a.attempt_id),
                serde_json::to_value(&report)?,
            )
            .await?;
        ensure!(
            status == StatusCode::OK && v["state"] == "complete",
            "scanner report not acknowledged (HTTP {})",
            status.as_u16()
        );
        self.clear()?;
        Ok(
            json!({"state":"complete","attempt_id":a.attempt_id,"technical":report.technical,"content":"unknown","delivery":"not_released"}),
        )
    }
    async fn download(&self, a: &Assignment) -> Result<()> {
        self.clear_input()?;
        fs::create_dir(self.path("input"))?;
        fs::set_permissions(self.path("input"), fs::Permissions::from_mode(0o755))?;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path("input/asset"))?;
        let mut hash = Sha256::new();
        for ordinal in 0..a.chunk_count {
            self.renew(a).await?;
            let response = self
                .client
                .get(self.url(&format!(
                    "/v1/output-scans/attempts/{}/chunks/{ordinal}",
                    a.attempt_id
                )))
                .bearer_auth(&self.token)
                .send()
                .await
                .context("scanner chunk transport failed")?;
            ensure!(
                response.status() == StatusCode::OK,
                "scanner chunk access rejected"
            );
            let expected = (a.byte_length - ordinal * CHUNK).min(CHUNK);
            ensure!(
                response.content_length() == Some(expected),
                "scanner chunk length mismatch"
            );
            let bytes = body(response, expected).await?;
            ensure!(bytes.len() as u64 == expected, "truncated scanner chunk");
            hash.update(&bytes);
            f.write_all(&bytes)?;
        }
        ensure!(
            crate::governance::hex(&hash.finalize()) == a.sha256,
            "scanner whole-file digest mismatch"
        );
        f.sync_all()?;
        fs::set_permissions(self.path("input/asset"), fs::Permissions::from_mode(0o444))?;
        self.renew(a).await?;
        Ok(())
    }
    async fn renew(&self, a: &Assignment) -> Result<()> {
        self.config.manifest.validate()?;
        let (status, _) = self
            .json(
                Method::POST,
                &format!("/v1/output-scans/attempts/{}/renew", a.attempt_id),
                json!({}),
            )
            .await?;
        ensure!(status == StatusCode::OK, "scan lease no longer authorized");
        Ok(())
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.api_origin.trim_end_matches('/'), path)
    }
    async fn json(&self, method: Method, path: &str, value: Value) -> Result<(StatusCode, Value)> {
        let response = self
            .client
            .request(method, self.url(path))
            .bearer_auth(&self.token)
            .json(&value)
            .send()
            .await
            .context("scanner API transport failed")?;
        let status = response.status();
        let bytes = body(response, JSON_LIMIT).await?;
        // Error bodies are never exposed or used as instructions; only numeric status is retained.
        if !status.is_success() {
            return Ok((status, Value::Null));
        }
        Ok((
            status,
            serde_json::from_slice(&bytes).context("invalid scanner API response")?,
        ))
    }
    fn clear_input(&self) -> Result<()> {
        if self.path("input").exists() {
            fs::remove_dir_all(self.path("input"))?;
        }
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        self.clear_input()?;
        if self.path("active.json").exists() {
            fs::remove_file(self.path("active.json"))?;
        }
        File::open(&self.config.work_dir)?.sync_all()?;
        Ok(())
    }
    fn ensure_evidence_capacity(&self) -> Result<()> {
        // No silent deletion of audit evidence. An operator archives it before this bounded store fills.
        let mut count = 0u64;
        let mut bytes = 0u64;
        for entry in fs::read_dir(&self.config.work_dir)? {
            let e = entry?;
            let m = e.metadata()?;
            if m.is_file() {
                count += 1;
                bytes = bytes.saturating_add(m.len());
            }
            ensure!(
                count <= 4096 && bytes <= 128 * 1024 * 1024,
                "scanner evidence store full; archive completed evidence before continuing"
            );
        }
        Ok(())
    }
}
async fn body(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        response.content_length().is_none_or(|n| n <= limit),
        "scanner response too large"
    );
    let mut bytes = Vec::new();
    while let Some(part) = response
        .chunk()
        .await
        .context("scanner response interrupted")?
    {
        ensure!(
            (bytes.len() + part.len()) as u64 <= limit,
            "scanner response too large"
        );
        bytes.extend_from_slice(&part);
    }
    Ok(bytes)
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_limited(path, JSON_LIMIT)?)?)
}
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "state must be a regular file"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "state file too large");
    Ok(bytes)
}
fn persist<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    persist_bytes(path, &serde_json::to_vec(value)?)
}
fn persist_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() as u64 <= JSON_LIMIT, "evidence too large");
    let temp = path.with_extension("pending");
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(path.parent().context("state has no parent")?)?.sync_all()?;
    Ok(())
}

pub async fn run(config: Config, token: String, once: bool) -> Result<()> {
    let worker = Worker::new(config, token)?;
    worker.preflight().await?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let signal = shutdown.clone();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = tokio::spawn(async move {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        signal.cancel();
    });
    loop {
        let result = tokio::select! {
            result = worker.once() => result,
            _ = shutdown.cancelled() => { worker.abort_current().await?; return Ok(()); }
        };
        match result {
            Ok(v) => {
                if once || v["state"] != "idle" {
                    println!("{}", v);
                }
                if once {
                    listener.abort();
                    return Ok(());
                }
            }
            Err(_) => {
                // Never print reqwest's URL, process output, API bodies or secret-bearing errors.
                eprintln!(
                    "scanner attempt paused; inspect credential, lease, pinned image and local capacity"
                );
                if once {
                    listener.abort();
                    bail!("scanner attempt did not complete");
                }
            }
        }
        tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_secs(15)) => {} }
    }
}
