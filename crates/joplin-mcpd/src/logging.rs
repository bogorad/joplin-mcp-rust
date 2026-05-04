use crate::config::LoggingConfig;
use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider};
use regex::Regex;
use std::{collections::HashMap, sync::OnceLock, time::Duration};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub const VICTORIALOGS_STREAM_FIELDS: &str =
    "service.name,service.instance.id,deployment.environment";

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

pub fn init_logging(config: &LoggingConfig) -> anyhow::Result<LogGuard> {
    if config.otlp_protocol != "http/protobuf" {
        anyhow::bail!("OTLP logs protocol must be http/protobuf");
    }

    validate_victorialogs_endpoint(config)?;

    let service_instance_id = Uuid::new_v4().to_string();
    let exporter = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(config.otlp_logs_endpoint.as_str())
        .with_timeout(Duration::from_secs(2))
        .with_headers(victorialogs_headers())
        .build()
        .context("build OTLP HTTP protobuf log exporter")?;
    let provider = SdkLoggerProvider::builder()
        .with_resource(logging_resource(config, &service_instance_id))
        .with_batch_exporter(exporter)
        .build();

    let otel_layer = OpenTelemetryTracingBridge::new(&provider);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
        .context("install tracing subscriber")?;

    tracing::info!(
        service.name = %config.service_name,
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

pub fn logging_resource(config: &LoggingConfig, service_instance_id: &str) -> Resource {
    Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", config.service_name.clone()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("service.instance.id", service_instance_id.to_string()),
            KeyValue::new(
                "deployment.environment",
                config.deployment_environment.clone(),
            ),
        ])
        .build()
}

pub fn victorialogs_headers() -> HashMap<String, String> {
    HashMap::from([(
        "VL-Stream-Fields".to_string(),
        VICTORIALOGS_STREAM_FIELDS.to_string(),
    )])
}

pub fn validate_victorialogs_endpoint(config: &LoggingConfig) -> anyhow::Result<()> {
    if config.otlp_logs_endpoint.path() != "/insert/opentelemetry/v1/logs" {
        anyhow::bail!("logging.otlp_logs_endpoint must end with /insert/opentelemetry/v1/logs");
    }
    if config.email_display != "redacted" {
        anyhow::bail!("logging.email_display must be redacted");
    }
    Ok(())
}

pub fn redact_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "redacted".to_string();
    };
    let mut chars = local.chars();
    let first = chars.next().unwrap_or('*');
    format!("{first}***@{domain}")
}

pub fn redact_auth_header(_value: &str) -> &'static str {
    "Authorization: [redacted]"
}

pub fn redact_password(_value: &str) -> &'static str {
    "[redacted-password]"
}

pub fn redact_token(_value: &str) -> &'static str {
    "[redacted-token]"
}

pub fn redact_token_hash(_value: &str) -> &'static str {
    "[redacted-token-hash]"
}

pub fn redact_note_body(_value: &str) -> &'static str {
    "[redacted-note-body]"
}

pub fn redact_resource_binary(_value: &[u8]) -> &'static str {
    "[redacted-resource-binary]"
}

pub fn sanitize_error_text(input: &str) -> String {
    let mut output = input.to_string();
    for (regex, replacement) in [
        (auth_header_regex(), "Authorization: [redacted]".to_string()),
        (password_regex(), "password=[redacted-password]".to_string()),
        (
            token_hash_regex(),
            "token_hash=[redacted-token-hash]".to_string(),
        ),
        (token_regex(), "token=[redacted-token]".to_string()),
        (
            note_body_regex(),
            "note_body=[redacted-note-body]".to_string(),
        ),
        (
            resource_binary_regex(),
            "resource_binary=[redacted-resource-binary]".to_string(),
        ),
    ] {
        output = regex.replace_all(&output, replacement).into_owned();
    }
    output
}

fn auth_header_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)Authorization:\\s*Bearer\\s+\\S+").expect("valid regex"))
}

fn password_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)password=\\S+").expect("valid regex"))
}

fn token_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)\\btoken=\\S+").expect("valid regex"))
}

fn token_hash_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)token_hash=\\S+").expect("valid regex"))
}

fn note_body_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)note_body=\\S+").expect("valid regex"))
}

fn resource_binary_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new("(?i)resource_binary=\\S+").expect("valid regex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email_display() {
        assert_eq!(redact_email("alice@example.com"), "a***@example.com");
        assert_eq!(redact_email("not-an-email"), "redacted");
    }

    #[test]
    fn sanitizes_common_secret_markers() {
        let text = sanitize_error_text(
            "password=secret token=abc token_hash=hash note_body=body resource_binary=bytes Authorization: Bearer mcp_raw",
        );
        assert!(!text.contains("password=secret"));
        assert!(!text.contains("token=abc"));
        assert!(!text.contains("token_hash=hash"));
        assert!(!text.contains("note_body=body"));
        assert!(!text.contains("resource_binary=bytes"));
        assert!(!text.contains("Bearer mcp_raw"));
    }

    #[test]
    fn redaction_helpers_do_not_echo_sensitive_inputs() {
        assert_eq!(
            redact_auth_header("Authorization: Bearer raw"),
            "Authorization: [redacted]"
        );
        assert_eq!(redact_password("secret"), "[redacted-password]");
        assert_eq!(redact_token("raw-token"), "[redacted-token]");
        assert_eq!(redact_token_hash("hash"), "[redacted-token-hash]");
        assert_eq!(redact_note_body("note body"), "[redacted-note-body]");
        assert_eq!(
            redact_resource_binary(b"bytes"),
            "[redacted-resource-binary]"
        );
    }

    #[test]
    fn victorialogs_stream_fields_are_low_cardinality() {
        let headers = victorialogs_headers();
        assert_eq!(
            headers.get("VL-Stream-Fields").expect("stream fields"),
            VICTORIALOGS_STREAM_FIELDS
        );
        for forbidden in ["user", "email", "token", "request", "test"] {
            assert!(!VICTORIALOGS_STREAM_FIELDS.contains(forbidden));
        }
    }

    #[test]
    fn validates_victorialogs_endpoint_and_redacted_email_display() {
        let mut config = LoggingConfig::default();
        validate_victorialogs_endpoint(&config).expect("default logging config is valid");

        config.otlp_logs_endpoint = "http://victorialogs.lan:9428/v1/logs"
            .parse()
            .expect("valid url");
        let error = validate_victorialogs_endpoint(&config).expect_err("wrong path rejected");
        assert!(error.to_string().contains("/insert/opentelemetry/v1/logs"));

        config = LoggingConfig::default();
        config.email_display = "raw".to_string();
        let error = validate_victorialogs_endpoint(&config).expect_err("raw email rejected");
        assert!(error.to_string().contains("email_display"));
    }
}
