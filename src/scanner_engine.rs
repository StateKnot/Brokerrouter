//! Fixed-command, content-addressed OCI detector. No credentials or network enter containers.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, sync::OnceLock};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

pub const CONTRACT: &str = "clamav-ffmpeg-mp4-v1";
pub const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_OUTPUT: u64 = 64 * 1024;
const STEPS: [&str; 3] = ["antivirus", "probe", "decode"];

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub contract: String,
    /// Local image ID or registry digest, never a mutable tag. Pulled by deployment, not the worker.
    pub image: String,
    pub worker_sha256: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        let digest = self.image.rsplit_once("sha256:").map(|(_, d)| d);
        ensure!(
            self.contract == CONTRACT
                && self.worker_sha256 == executable_digest()?
                && self.image.len() <= 256
                && (self.image.starts_with("sha256:") || self.image.contains("@sha256:"))
                && self
                    .image
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"/._:@-".contains(&c))
                && digest.is_some_and(hash_text)
                && self.issued_at <= Utc::now() + chrono::Duration::seconds(5)
                && self.expires_at > Utc::now()
                && self.expires_at <= self.issued_at + chrono::Duration::days(7),
            "invalid, expired or unpinned detector manifest"
        );
        Ok(())
    }
    pub fn revision(&self) -> Result<String> {
        self.validate()?;
        Ok(hex_hash(&serde_json::to_vec(self)?))
    }
}
pub fn hash_text(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn hex_hash(bytes: &[u8]) -> String {
    crate::governance::hex(&crate::store::hash(bytes))
}
pub fn executable_digest() -> Result<String> {
    static HASH: OnceLock<String> = OnceLock::new();
    if let Some(h) = HASH.get() {
        return Ok(h.clone());
    }
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(HASH
        .get_or_init(|| crate::governance::hex(&hash.finalize()))
        .clone())
}

#[derive(Clone)]
pub struct Engine {
    pub docker_binary: std::path::PathBuf,
    pub docker_socket: std::path::PathBuf,
    pub docker_config: std::path::PathBuf,
    pub manifest: Manifest,
}
#[derive(Serialize)]
pub struct Finding {
    pub technical: crate::output_scans::Technical,
    pub reason_code: &'static str,
    pub evidence: Value,
}
#[derive(Serialize)]
struct Run {
    exit_code: Option<i32>,
    stdout_sha256: String,
    stderr_sha256: String,
    boundary: &'static str,
    #[serde(skip)]
    stdout: Vec<u8>,
    #[serde(skip)]
    stderr: Vec<u8>,
}
impl Engine {
    fn docker(&self) -> Command {
        let mut c = Command::new(&self.docker_binary);
        // Do not inherit scanner/API secrets, proxy variables, Docker contexts or credential helpers.
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("--config")
            .arg(&self.docker_config)
            .arg("--host")
            .arg(format!("unix://{}", self.docker_socket.display()))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        c
    }
    pub async fn cleanup(&self, attempt: Uuid) -> Result<()> {
        for step in STEPS {
            self.remove(&format!("br-scan-{attempt}-{step}")).await?;
        }
        Ok(())
    }
    async fn remove(&self, name: &str) -> Result<()> {
        let r = bounded(self.docker().args(["rm", "--force", name]), 15).await?;
        // A missing container is expected after --rm; a disconnected daemon is not.
        ensure!(
            r.boundary == "exited"
                && (r.exit_code == Some(0)
                    || (r.exit_code == Some(1)
                        && String::from_utf8_lossy(&r.stderr).contains("No such container"))),
            "detector container cleanup failed"
        );
        Ok(())
    }
    pub async fn preflight(&self) -> Result<()> {
        self.manifest.validate()?;
        let r = bounded(
            self.docker().args([
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                &self.manifest.image,
            ]),
            15,
        )
        .await?;
        ensure!(
            r.exit_code == Some(0) && r.boundary == "exited",
            "pinned detector image is not locally available"
        );
        Ok(())
    }
    pub async fn step(&self, attempt: Uuid, dir: &Path, step: &str) -> Result<Value> {
        self.manifest.validate()?;
        ensure!(STEPS.contains(&step), "invalid detector step");
        let directory = dir.canonicalize()?;
        let source = directory.to_str().context("invalid detector directory")?;
        ensure!(!source.contains([',', '\n', '\r']), "invalid mount path");
        let name = format!("br-scan-{attempt}-{step}");
        self.remove(&name).await?;
        let mut c = self.docker();
        c.args([
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges=true",
            "--user=65532:65532",
            "--pids-limit=64",
            "--memory=2g",
            "--memory-swap=2g",
            "--cpus=1",
            "--ulimit=nofile=128:128",
            "--ulimit=core=0:0",
            "--ulimit=fsize=1073741824:1073741824",
            "--log-driver=none",
            "--init",
            "--name",
            &name,
            "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=1073741824,mode=1777",
            "--mount",
            &format!("type=bind,source={source},target=/input,readonly"),
            "--entrypoint",
            "/usr/bin/timeout",
            &self.manifest.image,
            "--signal=KILL",
            "120",
            match step {
                "antivirus" => "/usr/bin/clamscan",
                "probe" => "/usr/bin/ffprobe",
                _ => "/usr/bin/ffmpeg",
            },
        ]);
        match step {
            "antivirus" => {
                c.args([
                    "--database=/opt/clamav",
                    "--no-summary",
                    "--stdout",
                    "--disable-cache",
                    "--max-filesize=512M",
                    "--max-scansize=1024M",
                    "--max-files=1000",
                    "--max-recursion=8",
                    "--max-scantime=90000",
                    "--alert-exceeds-max=yes",
                    "--alert-encrypted=yes",
                    "--heuristic-alerts=yes",
                    "--tempdir=/tmp",
                    "/input/asset",
                ]);
            }
            "probe" => {
                c.args([
                "-v", "error", "-protocol_whitelist", "file", "-format_whitelist", "mov",
                "-codec_whitelist", "h264,hevc,aac", "-threads", "1",
                "-show_entries", "format=format_name,duration:format_tags=major_brand:stream=codec_name,codec_type,width,height,avg_frame_rate,sample_rate,channels",
                "-of", "json", "/input/asset",
            ]);
            }
            _ => {
                c.args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-xerror",
                    "-err_detect",
                    "explode",
                    "-protocol_whitelist",
                    "file",
                    "-format_whitelist",
                    "mov",
                    "-codec_whitelist",
                    "h264,hevc,aac",
                    "-threads",
                    "1",
                    "-i",
                    "/input/asset",
                    "-map",
                    "0",
                    "-threads",
                    "1",
                    "-filter_threads",
                    "1",
                    "-filter_complex_threads",
                    "1",
                    "-f",
                    "null",
                    "-",
                    "-progress",
                    "pipe:1",
                    "-nostats",
                ]);
            }
        }
        // A second watchdog lives inside the container and survives worker/client crashes.
        let run = bounded(&mut c, 130).await;
        // Killing the Docker client alone does NOT kill its container.
        self.remove(&name).await?;
        let r = run?;
        let outcome = if r.boundary != "exited" || matches!(r.exit_code, None | Some(124..=137)) {
            "error"
        } else if step == "antivirus" {
            match r.exit_code {
                Some(1) => "unsafe",
                Some(0) if r.stderr.is_empty() && r.stdout == b"/input/asset: OK\n" => "passed",
                _ => "error",
            }
        } else if r.exit_code != Some(0) || !r.stderr.is_empty() {
            "unsupported"
        } else if step == "probe" {
            if valid_probe(&r.stdout) {
                "passed"
            } else {
                "unsupported"
            }
        } else if valid_decode(&r.stdout) {
            "passed"
        } else {
            "unsupported"
        };
        Ok(json!({"step":step,"outcome":outcome,"process":r}))
    }
}

async fn bounded(command: &mut Command, seconds: u64) -> Result<Run> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start isolated detector")?;
    let stdout = child.stdout.take().context("missing stdout")?;
    let stderr = child.stderr.take().context("missing stderr")?;
    async fn read(input: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        input.take(MAX_OUTPUT + 1).read_to_end(&mut out).await?;
        ensure!(
            out.len() as u64 <= MAX_OUTPUT,
            "detector output limit exceeded"
        );
        Ok(out)
    }
    let run = tokio::time::timeout(Duration::from_secs(seconds), async {
        tokio::try_join!(read(stdout), read(stderr), async {
            Ok(child.wait().await?)
        })
    })
    .await;
    let (exit_code, stdout, stderr, boundary) = match run {
        Ok(Ok((o, e, status))) => (status.code(), o, e, "exited"),
        other => {
            let _ = child.kill().await;
            (
                None,
                Vec::new(),
                Vec::new(),
                if other.is_err() {
                    "timeout"
                } else {
                    "output_limit_or_io"
                },
            )
        }
    };
    Ok(Run {
        exit_code,
        stdout_sha256: hex_hash(&stdout),
        stderr_sha256: hex_hash(&stderr),
        boundary,
        stdout,
        stderr,
    })
}
fn valid_probe(raw: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(raw) else {
        return false;
    };
    let Some(streams) = v["streams"].as_array() else {
        return false;
    };
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|x| x.parse::<f64>().ok());
    let brand = v["format"]["tags"]["major_brand"].as_str();
    if !duration.is_some_and(|x| x.is_finite() && x > 0.0 && x <= 60.0)
        || !brand.is_some_and(|b| {
            [
                "isom", "iso2", "iso4", "iso5", "iso6", "mp41", "mp42", "avc1", "dash",
            ]
            .contains(&b)
        })
        || v["format"]["format_name"] != "mov,mp4,m4a,3gp,3g2,mj2"
        || streams.is_empty()
        || streams.len() > 2
    {
        return false;
    }
    let mut videos = 0;
    let mut audios = 0;
    for s in streams {
        match s["codec_type"].as_str() {
            Some("video") => {
                videos += 1;
                let w = s["width"].as_u64().unwrap_or(0);
                let h = s["height"].as_u64().unwrap_or(0);
                let fps = s["avg_frame_rate"]
                    .as_str()
                    .and_then(|f| f.split_once('/'))
                    .and_then(|(a, b)| Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?));
                if ![Some("h264"), Some("hevc")].contains(&s["codec_name"].as_str())
                    || w == 0
                    || h == 0
                    || w > 3840
                    || h > 3840
                    || w.saturating_mul(h) > 3840 * 2160
                    || !fps.is_some_and(|f| f.is_finite() && f > 0.0 && f <= 60.0)
                {
                    return false;
                }
            }
            Some("audio") => {
                audios += 1;
                let rate = s["sample_rate"]
                    .as_str()
                    .and_then(|x| x.parse::<u64>().ok());
                if s["codec_name"] != "aac"
                    || !rate.is_some_and(|x| (8000..=96000).contains(&x))
                    || !s["channels"].as_u64().is_some_and(|n| (1..=2).contains(&n))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    videos == 1 && audios <= 1
}
fn valid_decode(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let mut frames = None;
    let mut micros = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("frame=") {
            frames = v.trim().parse::<u64>().ok();
        }
        if let Some(v) = line.strip_prefix("out_time_us=") {
            micros = v.trim().parse::<u64>().ok();
        }
    }
    text.ends_with("progress=end\n")
        && frames.is_some_and(|f| (1..=3600).contains(&f))
        && micros.is_some_and(|t| (1..=60_100_000).contains(&t))
}
pub fn finding(steps: &[Value]) -> Result<Finding> {
    use crate::output_scans::Technical;
    ensure!(
        !steps.is_empty() && steps.len() <= 3,
        "missing detector evidence"
    );
    for (i, step) in steps.iter().enumerate() {
        ensure!(step["step"] == STEPS[i], "out-of-order detector evidence");
        let (technical, reason_code) = match step["outcome"].as_str() {
            Some("passed") => continue,
            Some("unsafe") if i == 0 => (Technical::Unsafe, "antivirus_alert"),
            Some("unsupported") => (Technical::Unsupported, "media_contract_rejected"),
            Some("error") => (Technical::Error, "detector_failed"),
            _ => bail!("invalid detector evidence"),
        };
        return Ok(Finding {
            technical,
            reason_code,
            evidence: json!(steps),
        });
    }
    ensure!(steps.len() == 3, "incomplete detector pipeline");
    Ok(Finding {
        technical: Technical::Clean,
        reason_code: "semantic_not_run",
        evidence: json!(steps),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_requires_exact_image_and_bounded_freshness() {
        let mut m = Manifest {
            contract: CONTRACT.into(),
            image: format!("sha256:{}", "a".repeat(64)),
            worker_sha256: executable_digest().unwrap(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::days(1),
        };
        assert!(m.validate().is_ok());
        m.image = "debian:latest".into();
        assert!(m.validate().is_err());
        m.image = format!("sha256:{}", "a".repeat(64));
        m.expires_at = Utc::now() + chrono::Duration::days(8);
        assert!(m.validate().is_err());
        m.expires_at = Utc::now() - chrono::Duration::seconds(1);
        assert!(m.validate().is_err());
    }
    #[test]
    fn media_contract_is_fail_closed_and_requires_full_decode() {
        let mut v = json!({"format":{"duration":"1.0","format_name":"mov,mp4,m4a,3gp,3g2,mj2","tags":{"major_brand":"isom"}},"streams":[{"codec_type":"video","codec_name":"h264","width":64,"height":64,"avg_frame_rate":"25/1"}]});
        assert!(valid_probe(&serde_json::to_vec(&v).unwrap()));
        v["streams"][0]["avg_frame_rate"] = json!("1/0");
        assert!(!valid_probe(&serde_json::to_vec(&v).unwrap()));
        assert!(valid_decode(
            b"frame=25\nout_time_us=1000000\nprogress=end\n"
        ));
        assert!(!valid_decode(
            b"frame=25\nout_time_us=1000000\nprogress=continue\n"
        ));
        assert!(!valid_decode(
            b"frame=999999\nout_time_us=1000000\nprogress=end\n"
        ));
        assert!(finding(&[json!({"step":"antivirus","outcome":"passed"})]).is_err());
    }
    #[tokio::test]
    async fn process_time_and_output_budgets_fail_closed() {
        let excessive = bounded(
            Command::new("/bin/sh")
                .args(["-c", "while :; do printf 'bounded-output-fixture\n'; done"]),
            5,
        )
        .await
        .unwrap();
        assert_eq!(excessive.boundary, "output_limit_or_io");
        assert!(excessive.exit_code.is_none());
        let timeout = bounded(
            Command::new("/bin/sh").args(["-c", "while :; do :; done"]),
            1,
        )
        .await
        .unwrap();
        assert_eq!(timeout.boundary, "timeout");
        assert!(timeout.exit_code.is_none());
    }
}
