use brokerrouter::{
    scanner_engine::{CONTRACT, Engine, Manifest},
    scanner_worker::Config,
};
use chrono::{Duration, Utc};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};
use uuid::Uuid;

pub struct Local {
    pub config: Config,
    pub _socket: Option<UnixListener>,
}
impl Local {
    pub fn new(origin: String, real: bool) -> Self {
        // macOS's per-user temp path can exceed Unix-domain socket's 104-byte path limit.
        let root = PathBuf::from("/tmp").join(format!("br-detector-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let root = root.canonicalize().unwrap();
        fs::create_dir(root.join("docker-config")).unwrap();
        fs::set_permissions(
            root.join("docker-config"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::write(root.join("docker-config/config.json"), b"{}").unwrap();
        let (binary, socket, image, listener) = if real {
            (
                PathBuf::from(std::env::var("BROKERROUTER_TEST_DOCKER_BINARY").unwrap()),
                PathBuf::from(std::env::var("BROKERROUTER_TEST_DOCKER_SOCKET").unwrap()),
                std::env::var("BROKERROUTER_TEST_DETECTOR_IMAGE").unwrap(),
                None,
            )
        } else {
            let binary = root.join("fake-docker");
            fs::copy(
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/detector/fake-docker.sh"),
                &binary,
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            let socket = root.join("docker.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            (
                binary,
                socket,
                format!("sha256:{}", "a".repeat(64)),
                Some(listener),
            )
        };
        Self {
            config: Config {
                api_origin: origin,
                docker_binary: binary,
                docker_socket: socket,
                work_dir: root,
                manifest: Manifest {
                    contract: CONTRACT.into(),
                    image,
                    worker_sha256: brokerrouter::scanner_engine::executable_digest().unwrap(),
                    issued_at: Utc::now(),
                    expires_at: Utc::now() + Duration::days(1),
                },
            },
            _socket: listener,
        }
    }
    pub fn engine(&self) -> Engine {
        Engine {
            docker_binary: self.config.docker_binary.clone(),
            docker_socket: self.config.docker_socket.clone(),
            docker_config: self.config.work_dir.join("docker-config"),
            manifest: self.config.manifest.clone(),
        }
    }
    pub fn executions(&self) -> usize {
        fs::read_to_string(self.config.work_dir.join("executions"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    pub async fn video(&self) -> Vec<u8> {
        let output = self.config.work_dir.join("fixture");
        fs::create_dir(&output).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o777)).unwrap();
        let result = tokio::process::Command::new(&self.config.docker_binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("--host")
            .arg(format!("unix://{}", self.config.docker_socket.display()))
            .args([
                "run",
                "--rm",
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges=true",
                "--user=65532:65532",
                "--pids-limit=64",
                "--memory=512m",
                "--cpus=1",
                "--mount",
            ])
            .arg(format!("type=bind,source={},target=/out", output.display()))
            .args([
                "--entrypoint=/usr/bin/timeout",
                &self.config.manifest.image,
                "30",
                "/usr/bin/ffmpeg",
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=64x64:r=25",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
                "/out/sample.mp4",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fs::read(output.join("sample.mp4")).unwrap()
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.config.work_dir);
    }
}
