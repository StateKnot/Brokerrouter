use brokerrouter::{api, connect, migrate, money, store};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;
use std::{net::SocketAddr, str::FromStr};
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Migrate,
    ServeBff {
        #[arg(long, default_value = "127.0.0.1:8087")]
        bind: SocketAddr,
    },
    BffIdentityBind {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        user: Uuid,
        #[arg(long)]
        issuer: String,
        #[arg(long)]
        subject: String,
    },
    BffIdentitySet {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, action=clap::ArgAction::Set)]
        active: bool,
        #[arg(long)]
        reason: String,
    },
    BffMaintain,
    ServeConsole {
        #[arg(long, default_value = "127.0.0.1:8086")]
        bind: SocketAddr,
    },
    ServeCatalog {
        #[arg(long, default_value = "127.0.0.1:8088")]
        bind: SocketAddr,
    },
    ConsolePermissionSet {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        user: Uuid,
        #[arg(long, value_parser=["usage_read","audit_read","catalog_read","catalog_write"])]
        permission: String,
        #[arg(long, action=clap::ArgAction::Set)]
        enabled: bool,
        #[arg(long)]
        reason: String,
    },
    ServeOutputDelivery {
        #[arg(long, default_value = "127.0.0.1:8085")]
        bind: SocketAddr,
    },
    ServeOutputScans {
        #[arg(long, default_value = "127.0.0.1:8084")]
        bind: SocketAddr,
    },
    OutputScanProfilePublish {
        #[arg(long)]
        file: std::path::PathBuf,
    },
    OutputScanProfileDisable {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
    },
    OutputScannerTokenCreate {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        profile: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        expires_at: DateTime<Utc>,
    },
    OutputScannerTokenRevoke {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
    },
    ServeMedia {
        #[arg(long, default_value = "127.0.0.1:8083")]
        bind: SocketAddr,
    },
    MediaWorker {
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
    OutputWorker {
        #[arg(long, default_value_t = 2)]
        concurrency: usize,
    },
    MediaEndpointAdd {
        #[arg(long)]
        file: std::path::PathBuf,
    },
    MediaEndpointDisable {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
    },
    MediaAttachTask {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        provider_task_id: String,
        #[arg(long)]
        evidence: String,
    },
    MediaReconcile {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        usage_file: std::path::PathBuf,
        #[arg(long)]
        evidence: String,
    },
    ServeAssets {
        #[arg(long, default_value = "127.0.0.1:8082")]
        bind: SocketAddr,
    },
    AssetQuotaSet {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        key: Option<Uuid>,
        #[arg(long)]
        bytes: i64,
    },
    ServeControl {
        #[arg(long, default_value = "127.0.0.1:8081")]
        bind: SocketAddr,
    },
    GovernanceUserAdd {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        issuer: String,
        #[arg(long)]
        subject: String,
    },
    RuntimeBind {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        key: Uuid,
        #[arg(long)]
        user: Uuid,
        #[arg(long)]
        runtime: String,
        #[arg(long)]
        agent: String,
    },
    GuardrailPublish {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        file: std::path::PathBuf,
    },
    PolicyPut {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        file: std::path::PathBuf,
    },
    RequestReconcile {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        evidence: String,
        #[arg(long)]
        no_charge: bool,
        #[arg(long)]
        input_tokens: Option<u64>,
        #[arg(long)]
        output_tokens: Option<u64>,
    },
    Serve {
        #[arg(long, env = "BROKERROUTER_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
    },
    TenantCreate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        budget_cny: String,
    },
    ModelPut {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
    },
    KeyCreate {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        budget_cny: String,
        #[arg(long, required = true)]
        model: Vec<String>,
        #[arg(long)]
        expires_at: Option<DateTime<Utc>>,
    },
    KeyRevoke {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        id: Uuid,
    },
    FxSet {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        usd_cny: String,
        #[arg(long, default_value = "manual")]
        source: String,
        #[arg(long)]
        effective_at: Option<DateTime<Utc>>,
    },
    FxShow {
        #[arg(long)]
        tenant: Uuid,
    },
    FxHistory {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        before: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    EndpointAdd {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        model: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        upstream_model: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        credential_env: String,
        #[arg(long, default_value = "CNY")]
        currency: String,
        #[arg(long)]
        input_per_million: String,
        #[arg(long)]
        output_per_million: String,
        #[arg(long)]
        context_tokens: i32,
        #[arg(long)]
        max_output_tokens: i32,
        #[arg(long, default_value_t = 100)]
        priority: i32,
        #[arg(long)]
        allow_private: bool,
        #[arg(long, default_value="chat", value_parser=["chat","embedding"])]
        operation: String,
        #[arg(long)]
        supports_tools: bool,
        #[arg(long)]
        embedding_dimension: Option<i32>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "brokerrouter=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let pool = connect(&cli.database_url).await.map_err(|_| {
        anyhow::anyhow!("database connection failed; verify DATABASE_URL and network")
    })?;
    match cli.command {
        Command::ServeBff { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let service = brokerrouter::bff::Bff::from_env(pool.clone())?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            let maintenance_pool = pool.clone();
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if brokerrouter::bff::maintain(&maintenance_pool)
                        .await
                        .is_err()
                    {
                        tracing::error!(
                            error_code = "bff_maintenance_failed",
                            "BFF expiry cleanup failed"
                        );
                    }
                }
            });
            tracing::info!(address=%bind,"OIDC console BFF listening; TLS termination and same-origin UI required");
            let result = axum::serve(listener, brokerrouter::bff::router(service))
                .with_graceful_shutdown(shutdown())
                .await;
            maintenance.abort();
            result?;
        }
        Command::BffIdentityBind {
            tenant,
            user,
            issuer,
            subject,
        } => {
            println!(
                "{}",
                brokerrouter::bff::bind_identity(&pool, tenant, user, &issuer, &subject).await?
            );
        }
        Command::BffIdentitySet {
            tenant,
            id,
            active,
            reason,
        } => {
            brokerrouter::bff::set_identity_active(&pool, tenant, id, active, &reason).await?;
            println!("OIDC identity binding updated");
        }
        Command::BffMaintain => {
            brokerrouter::bff::maintain(&pool).await?;
        }
        Command::ServeCatalog { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let service = brokerrouter::catalog_api::Catalog {
                pool: pool.clone(),
                verifier: std::sync::Arc::new(brokerrouter::human::Verifier::catalog_from_env()?),
                connections: std::sync::Arc::new(brokerrouter::catalog::connections_from_env()?),
            };
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"human catalog configuration listening; no provider credentials or approval authority");
            axum::serve(listener, brokerrouter::catalog_api::router(service))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Command::ServeConsole { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let service = brokerrouter::console_api::Console {
                pool: pool.clone(),
                verifier: std::sync::Arc::new(brokerrouter::human::Verifier::console_from_env()?),
            };
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"read-only console reports listening; no provider, decryption or approval credentials");
            axum::serve(listener, brokerrouter::console_api::router(service))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Command::ConsolePermissionSet {
            tenant,
            user,
            permission,
            enabled,
            reason,
        } => {
            let permission = serde_json::from_value(serde_json::Value::String(permission))?;
            brokerrouter::console::set_permission(
                &pool, tenant, user, permission, enabled, &reason,
            )
            .await?;
            println!("Console permission saved");
        }
        Command::ServeOutputDelivery { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let service = brokerrouter::output_delivery_api::Service {
                delivery: brokerrouter::output_delivery::Delivery::new(
                    brokerrouter::assets::Assets::from_env(pool.clone())?,
                ),
                verifier: std::sync::Arc::new(brokerrouter::human::Verifier::from_env()?),
            };
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"current-turn output delivery listening; human decisions remain on the separate control service");
            axum::serve(listener, brokerrouter::output_delivery_api::router(service))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Command::OutputScanProfilePublish { file } => {
            brokerrouter::output_scans::publish(
                &pool,
                serde_json::from_str(&std::fs::read_to_string(file)?)?,
            )
            .await?
        }
        Command::OutputScanProfileDisable { tenant, id } => {
            brokerrouter::output_scans::disable(&pool, tenant, id).await?
        }
        Command::OutputScannerTokenCreate {
            tenant,
            profile,
            name,
            expires_at,
        } => {
            let (id, token) =
                brokerrouter::output_scans::create_token(&pool, tenant, profile, &name, expires_at)
                    .await?;
            println!(
                "{}",
                serde_json::json!({"id":id,"token":token,"notice":"Shown once. Store only in the isolated scanner secret store, never in an Agent environment."})
            );
        }
        Command::OutputScannerTokenRevoke { tenant, id } => {
            brokerrouter::output_scans::revoke_token(&pool, tenant, id).await?
        }
        Command::ServeOutputScans { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let scans = brokerrouter::output_scans::Scans::new(
                brokerrouter::assets::Assets::from_env(pool.clone())?,
            );
            let worker = scans.clone();
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if worker.maintain().await.is_err() {
                        tracing::error!("output scan maintenance failed");
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"isolated scanner evidence API listening; never expose credentials to Agents");
            axum::serve(listener, brokerrouter::output_scan_api::router(scans))
                .with_graceful_shutdown(shutdown())
                .await?;
            maintenance.abort();
        }
        Command::ServeControl { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let control = brokerrouter::governance_api::Control {
                pool: pool.clone(),
                verifier: std::sync::Arc::new(brokerrouter::human::Verifier::from_env()?),
                cipher: std::sync::Arc::new(brokerrouter::cipher::ResultCipher::from_env()?),
            };
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"human governance control listening; keep separate from Agent execution environment");
            let maintenance_pool = pool.clone();
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if brokerrouter::human::maintain(&maintenance_pool)
                        .await
                        .is_err()
                    {
                        tracing::error!("human governance maintenance failed");
                    }
                }
            });
            axum::serve(
                listener,
                brokerrouter::governance_api::control_router(control),
            )
            .with_graceful_shutdown(shutdown())
            .await?;
            maintenance.abort();
        }
        Command::GovernanceUserAdd {
            tenant,
            issuer,
            subject,
        } => println!(
            "{}",
            brokerrouter::human::add_user(&pool, tenant, &issuer, &subject).await?
        ),
        Command::RuntimeBind {
            tenant,
            key,
            user,
            runtime,
            agent,
        } => brokerrouter::human::bind_runtime(&pool, tenant, key, user, &runtime, &agent).await?,
        Command::GuardrailPublish { tenant, file } => {
            brokerrouter::human::publish_guardrail(
                &pool,
                tenant,
                serde_json::from_str(&std::fs::read_to_string(file)?)?,
            )
            .await?
        }
        Command::PolicyPut { tenant, file } => {
            brokerrouter::human::put_policy(
                &pool,
                tenant,
                serde_json::from_str(&std::fs::read_to_string(file)?)?,
            )
            .await?
        }
        Command::RequestReconcile {
            tenant,
            id,
            evidence,
            no_charge,
            input_tokens,
            output_tokens,
        } => {
            let usage = match (no_charge, input_tokens, output_tokens) {
                (true, None, None) => None,
                (false, Some(i), Some(o)) => Some((i, o)),
                _ => {
                    anyhow::bail!("specify --no-charge OR both --input-tokens and --output-tokens")
                }
            };
            brokerrouter::attempts::reconcile(&pool, tenant, id, usage, &evidence).await?;
            println!("Request reconciled; no model request was sent");
        }
        Command::Migrate => {
            migrate(&pool).await?;
            println!("Migrations applied");
        }
        Command::MediaEndpointAdd { file } => {
            brokerrouter::media::put_endpoint(
                &pool,
                serde_json::from_str(&std::fs::read_to_string(file)?)?,
            )
            .await?
        }
        Command::MediaEndpointDisable { tenant, id } => {
            brokerrouter::media::disable_endpoint(&pool, tenant, id).await?
        }
        Command::MediaAttachTask {
            tenant,
            id,
            provider_task_id,
            evidence,
        } => {
            brokerrouter::media::attach_task(&pool, tenant, id, &provider_task_id, &evidence)
                .await?
        }
        Command::MediaReconcile {
            tenant,
            id,
            usage_file,
            evidence,
        } => {
            brokerrouter::media::reconcile(
                &pool,
                tenant,
                id,
                &serde_json::from_str::<Vec<money::UnitUsage>>(&std::fs::read_to_string(
                    usage_file,
                )?)?,
                &evidence,
            )
            .await?
        }
        Command::ServeMedia { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let m = brokerrouter::media::Media::from_env(pool.clone()).await?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"brokerrouter media API listening; worker runs separately");
            axum::serve(listener, brokerrouter::media_api::router(m))
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Command::OutputWorker { concurrency } => {
            if !(1..=8).contains(&concurrency) {
                anyhow::bail!("output concurrency must be 1..8");
            }
            brokerrouter::check_schema(&pool).await?;
            let outputs = brokerrouter::outputs::Outputs::from_env(pool.clone())?;
            let stop = tokio_util::sync::CancellationToken::new();
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..concurrency {
                let (worker, stop) = (outputs.clone(), stop.clone());
                workers.spawn(async move {
                    while !stop.is_cancelled() {
                        if !matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(630),
                                worker.tick()
                            )
                            .await,
                            Ok(Ok(_))
                        ) {
                            tracing::error!("output import pass failed; durable state retained");
                        }
                        tokio::select! {
                            _ = stop.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                    }
                });
            }
            let failed = tokio::select! {
                _ = shutdown() => false,
                _ = workers.join_next() => true,
            };
            stop.cancel();
            while workers.join_next().await.is_some() {}
            if failed {
                anyhow::bail!("output worker exited unexpectedly; restart with durable leases");
            }
        }
        Command::MediaWorker { concurrency } => {
            if !(1..=16).contains(&concurrency) {
                anyhow::bail!("media concurrency must be 1..16");
            }
            brokerrouter::check_schema(&pool).await?;
            let m = brokerrouter::media::Media::from_env(pool.clone()).await?;
            let stop = tokio_util::sync::CancellationToken::new();
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..concurrency {
                let (worker, stop) = (m.clone(), stop.clone());
                workers.spawn(async move {
                    while !stop.is_cancelled() {
                        if !matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(840),
                                worker.tick()
                            )
                            .await,
                            Ok(Ok(_))
                        ) {
                            tracing::error!("media worker pass failed; durable lease will recover");
                        }
                        tokio::select! {
                            _ = stop.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                    }
                });
            }
            let failed = tokio::select! {
                _ = shutdown() => false,
                _ = workers.join_next() => true,
            };
            stop.cancel();
            while workers.join_next().await.is_some() {}
            if failed {
                anyhow::bail!("media worker exited unexpectedly; restart with durable leases");
            }
        }
        Command::AssetQuotaSet { tenant, key, bytes } => {
            brokerrouter::assets::set_quota(&pool, tenant, key, bytes).await?;
        }
        Command::ServeAssets { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let assets = brokerrouter::assets::Assets::from_env(pool.clone())?;
            let worker = assets.clone();
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if worker.maintain().await.is_err() {
                        tracing::error!("asset maintenance failed");
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"brokerrouter assets listening");
            axum::serve(listener, brokerrouter::asset_api::router(assets.clone()))
                .with_graceful_shutdown(shutdown())
                .await?;
            assets.drain().await;
            maintenance.abort();
        }
        Command::Serve { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let gateway = brokerrouter::gateway::Gateway::from_env(pool.clone()).await?;
            let maintenance_pool = pool.clone();
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if brokerrouter::attempts::maintain(&maintenance_pool)
                        .await
                        .is_err()
                    {
                        tracing::error!("request maintenance failed");
                    }
                    if brokerrouter::governance::maintain(&maintenance_pool)
                        .await
                        .is_err()
                    {
                        tracing::error!("governance maintenance failed");
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"brokerrouter listening");
            axum::serve(listener, api::gateway_router(gateway.clone()))
                .with_graceful_shutdown(shutdown())
                .await?;
            gateway.drain().await;
            maintenance.abort();
        }
        Command::TenantCreate { name, budget_cny } => println!(
            "{}",
            store::create_tenant(&pool, &name, money::cny_micros(&budget_cny)?).await?
        ),
        Command::ModelPut { tenant, id, name } => {
            store::put_model(&pool, tenant, &id, &name).await?;
            println!("Model saved");
        }
        Command::KeyCreate {
            tenant,
            name,
            budget_cny,
            model,
            expires_at,
        } => {
            let (id, key) = store::create_key(
                &pool,
                tenant,
                &name,
                money::cny_micros(&budget_cny)?,
                &model,
                expires_at,
            )
            .await?;
            println!(
                "{}",
                serde_json::json!({"id":id,"key":key,"notice":"Store securely; shown only once"})
            );
        }
        Command::KeyRevoke { tenant, id } => {
            store::revoke_key(&pool, tenant, id).await?;
            println!("Key revoked");
        }
        Command::FxSet {
            tenant,
            usd_cny,
            source,
            effective_at,
        } => {
            let rate = Decimal::from_str(&usd_cny)?;
            let id = store::set_fx(
                &pool,
                tenant,
                rate,
                effective_at.unwrap_or_else(Utc::now),
                &source,
            )
            .await?;
            println!(
                "{}",
                serde_json::json!({"version":id,"primary_currency":"CNY","usd_cny":rate.to_string()})
            );
        }
        Command::FxShow { tenant } => println!(
            "{}",
            serde_json::to_string_pretty(&store::current_fx(&pool, tenant).await?)?
        ),
        Command::FxHistory {
            tenant,
            before,
            limit,
        } => println!(
            "{}",
            serde_json::to_string_pretty(&store::fx_history(&pool, tenant, before, limit).await?)?
        ),
        Command::EndpointAdd {
            tenant,
            model,
            name,
            upstream_model,
            base_url,
            credential_env,
            currency,
            input_per_million,
            output_per_million,
            context_tokens,
            max_output_tokens,
            priority,
            allow_private,
            operation,
            supports_tools,
            embedding_dimension,
        } => {
            let e = store::Endpoint {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                model_id: model,
                name,
                upstream_model,
                base_url,
                credential_env,
                allow_private,
                currency,
                input_per_million: money::nonnegative(&input_per_million)?,
                output_per_million: money::nonnegative(&output_per_million)?,
                context_tokens,
                max_output_tokens,
                priority,
                operation,
                supports_tools,
                embedding_dimension,
            };
            store::add_endpoint(&pool, &e).await?;
            println!("{}", e.id);
        }
    }
    pool.close().await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
