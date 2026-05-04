use crate::config::ClientLoggingConfig;
use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider};
use std::{collections::HashMap, time::Duration};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

const VICTORIALOGS_STREAM_FIELDS: &str = "service.name,service.instance.id,deployment.environment";

#[derive(Debug)]
pub struct LogGuard {
    provider: SdkLoggerProvider,
}

impl LogGuard {
    pub fn flush(&self) {
        if let Err(error) = self.provider.force_flush() {
            eprintln!("failed to flush OpenTelemetry logs: {error}");
        }
        if let Err(error) = self.provider.shutdown() {
            eprintln!("failed to shutdown OpenTelemetry logs: {error}");
        }
    }
}

pub fn init_logging(config: &ClientLoggingConfig) -> anyhow::Result<LogGuard> {
    if config.otlp_protocol != "http/protobuf" {
        anyhow::bail!("OTLP logs protocol must be http/protobuf");
    }
    if config.otlp_logs_endpoint.path() != "/insert/opentelemetry/v1/logs" {
        anyhow::bail!("OTLP logs endpoint must end with /insert/opentelemetry/v1/logs");
    }

    let service_instance_id = Uuid::new_v4().to_string();
    let exporter = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(config.otlp_logs_endpoint.as_str())
        .with_timeout(Duration::from_secs(2))
        .with_headers(HashMap::from([(
            "VL-Stream-Fields".to_string(),
            VICTORIALOGS_STREAM_FIELDS.to_string(),
        )]))
        .build()
        .context("build OTLP HTTP protobuf log exporter")?;
    let provider = SdkLoggerProvider::builder()
        .with_resource(
            Resource::builder_empty()
                .with_attributes([
                    KeyValue::new("service.name", "joplin-mcp-client"),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                    KeyValue::new("service.instance.id", service_instance_id.clone()),
                    KeyValue::new(
                        "deployment.environment",
                        config.deployment_environment.clone(),
                    ),
                ])
                .build(),
        )
        .with_batch_exporter(exporter)
        .build();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(OpenTelemetryTracingBridge::new(&provider))
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
        .context("install tracing subscriber")?;

    tracing::info!(
        service.name = "joplin-mcp-client",
        service.version = env!("CARGO_PKG_VERSION"),
        service.instance.id = %service_instance_id,
        deployment.environment = %config.deployment_environment,
        otlp.protocol = %config.otlp_protocol,
        otlp.logs.endpoint = %config.otlp_logs_endpoint,
        operation = "startup",
        outcome = "logging_initialized",
        "logging initialized"
    );

    Ok(LogGuard { provider })
}
