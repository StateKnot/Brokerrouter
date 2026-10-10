//! Real local TLS protocol fixtures, not production IdP or storage certification.
use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

const VARIABLES: [&str; 3] = [
    "BROKERROUTER_BFF_CA_FILE",
    "BROKERROUTER_ASSET_S3_CA_FILE",
    "BROKERROUTER_SCANNER_CA_FILE",
];

struct Fixture {
    root: std::path::PathBuf,
    server: Child,
    tls_port: u16,
    proxy_port: u16,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("brokerrouter-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("openssl")
                .current_dir(&root)
                .args(args)
                .output()
                .expect("TLS tests require openssl");
            assert!(output.status.success(), "openssl fixture generation failed");
        };
        for prefix in ["ca", "other"] {
            run(&[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=Brokerrouter Local Test CA",
                "-keyout",
                &format!("{prefix}.key"),
                "-out",
                &format!("{prefix}.pem"),
            ]);
        }
        run(&[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        ]);
        std::fs::write(
            root.join("server.ext"),
            "subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n",
        )
        .unwrap();
        run(&[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            "server.ext",
            "-out",
            "server.pem",
        ]);
        std::fs::write(root.join("empty.pem"), b" ").unwrap();
        std::fs::write(
            root.join("malformed.pem"),
            b"-----BEGIN CERTIFICATE-----\n???\n-----END CERTIFICATE-----",
        )
        .unwrap();
        std::fs::write(
            root.join("invalid-der.pem"),
            b"-----BEGIN CERTIFICATE-----\nYWJj\n-----END CERTIFICATE-----",
        )
        .unwrap();
        let certificate = std::fs::read(root.join("ca.pem")).unwrap();
        let mut bundle = certificate.clone();
        bundle.extend(std::fs::read(root.join("other.pem")).unwrap());
        std::fs::write(root.join("bundle.pem"), bundle).unwrap();
        let mut mixed = certificate.clone();
        mixed.extend(std::fs::read(root.join("ca.key")).unwrap());
        std::fs::write(root.join("mixed.pem"), mixed).unwrap();
        std::fs::write(
            root.join("too-many.pem"),
            certificate.repeat(MAX_CERTIFICATES + 1),
        )
        .unwrap();
        std::fs::write(root.join("oversized.pem"), vec![b' '; MAX_BUNDLE_BYTES + 1]).unwrap();
        let mut server = Command::new("python3")
            .args(["-u", "-c", include_str!("https_fixture.py")])
            .arg(root.join("server.pem"))
            .arg(root.join("server.key"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("TLS tests require Python 3 with stdlib ssl");
        let mut ready = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ports: [u16; 2] = serde_json::from_str(&ready).expect("HTTPS fixture must start");
        Self {
            root,
            server,
            tls_port: ports[0],
            proxy_port: ports[1],
        }
    }

    fn probe(&self, purpose: usize, mode: &str, ca: Option<&str>) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "tls_roots::tests::client_probe", "--nocapture"]);
        for variable in VARIABLES {
            command.env_remove(variable);
        }
        // Other TLS purposes must never silently configure this client. Native
        // roots still follow their existing platform defaults; isolate the fixture
        // from inherited native-root overrides instead of changing production trust.
        command.env("BROKERROUTER_MCP_CA_BUNDLE", self.root.join("ca.pem"));
        command.env("PGSSLROOTCERT", self.root.join("ca.pem"));
        command
            .env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR");
        if mode == "other-purpose" {
            command.env(VARIABLES[(purpose + 1) % 3], self.root.join("ca.pem"));
        }
        if let Some(ca) = ca {
            if ca.is_empty() {
                command.env(VARIABLES[purpose], "");
            } else {
                command.env(VARIABLES[purpose], self.root.join(ca));
            }
        }
        let proxy = format!("http://127.0.0.1:{}", self.proxy_port);
        for variable in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env(variable, &proxy);
        }
        command.env_remove("NO_PROXY").env_remove("no_proxy");
        command.env("BROKERROUTER_TLS_TEST_PURPOSE", purpose.to_string());
        command.env("BROKERROUTER_TLS_TEST_MODE", mode);
        command.env(
            "BROKERROUTER_TLS_TEST_ORIGIN",
            format!("https://localhost:{}", self.tls_port),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{purpose} / {mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        println!(
            "TLS purpose={purpose} case={mode} file={} passed",
            ca.unwrap_or("unset")
        );
    }
}

#[test]
fn purpose_scoped_private_ca_https_clients() {
    let fixture = Fixture::new();
    for purpose in 0..3 {
        fixture.probe(purpose, "correct", Some("ca.pem"));
        fixture.probe(purpose, "correct", Some("bundle.pem"));
        fixture.probe(purpose, "not-configured", None);
        fixture.probe(purpose, "other-purpose", None);
        fixture.probe(purpose, "wrong-ca", Some("other.pem"));
        fixture.probe(purpose, "wrong-hostname", Some("ca.pem"));
        for file in [
            "",
            ".",
            "empty.pem",
            "malformed.pem",
            "invalid-der.pem",
            "mixed.pem",
            "too-many.pem",
            "oversized.pem",
            "missing.pem",
        ] {
            fixture.probe(purpose, "invalid", Some(file));
        }
        let loaded = format!("loaded-{purpose}.pem");
        std::fs::copy(fixture.root.join("ca.pem"), fixture.root.join(&loaded)).unwrap();
        fixture.probe(purpose, "startup-only", Some(&loaded));
    }
}

// An isolated process supplies each startup environment without unsafe global
// environment mutation in concurrently running Rust/SQLx tests.
#[tokio::test]
async fn client_probe() {
    let Ok(purpose) = std::env::var("BROKERROUTER_TLS_TEST_PURPOSE") else {
        return;
    };
    let purpose: usize = purpose.parse().unwrap();
    let mode = std::env::var("BROKERROUTER_TLS_TEST_MODE").unwrap();
    let client = match purpose {
        0 => crate::bff::http_client(),
        1 => crate::assets::storage_http_client(),
        2 => crate::scanner_worker::http_client().map_err(|_| Error::Configuration),
        _ => panic!("invalid fixture purpose"),
    };
    if mode == "invalid" {
        assert!(
            client.is_err(),
            "invalid CA configuration must prevent startup"
        );
        return;
    }
    let client = client.unwrap();
    if mode == "startup-only" {
        std::fs::remove_file(std::env::var(VARIABLES[purpose]).unwrap()).unwrap();
    }
    let mut origin = std::env::var("BROKERROUTER_TLS_TEST_ORIGIN").unwrap();
    if mode == "wrong-hostname" {
        origin = origin.replace("localhost", "127.0.0.1");
    }
    let response = client.get(format!("{origin}/ok")).send().await;
    if !matches!(mode.as_str(), "correct" | "startup-only") {
        assert!(response.is_err(), "TLS must reject {mode}");
        return;
    }
    assert_eq!(response.unwrap().status(), reqwest::StatusCode::OK);
    let response = client
        .get(format!("{origin}/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::TEMPORARY_REDIRECT,
        "redirect must not be followed"
    );
    let stats: serde_json::Value = client
        .get(format!("{origin}/stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stats["proxy"], 0,
        "ambient proxy must not receive any request"
    );
    assert_eq!(
        stats["leak"], 0,
        "redirect target must not receive any request"
    );
}
