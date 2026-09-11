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
        Command::Migrate => {
            migrate(&pool).await?;
            println!("Migrations applied");
        }
        Command::Serve { bind } => {
            brokerrouter::check_schema(&pool).await?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(address=%bind,"brokerrouter listening");
            axum::serve(listener, api::router(pool.clone()))
                .with_graceful_shutdown(shutdown())
                .await?;
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
