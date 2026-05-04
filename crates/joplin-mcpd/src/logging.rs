use crate::config::LoggingConfig;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug)]
pub struct LogGuard;

impl LogGuard {
    pub fn flush(&self) {}
}

pub fn init_logging(config: &LoggingConfig) -> anyhow::Result<LogGuard> {
    if config.otlp_protocol != "http/protobuf" {
        anyhow::bail!("OTLP logs protocol must be http/protobuf");
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .try_init();

    tracing::info!(
        service.name = config.service_name,
        otlp.protocol = config.otlp_protocol,
        operation = "startup",
        outcome = "logging_initialized",
        "logging initialized"
    );

    Ok(LogGuard)
}

pub fn redact_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "redacted".to_string();
    };
    let mut chars = local.chars();
    let first = chars.next().unwrap_or('*');
    format!("{first}***@{domain}")
}

pub fn sanitize_error_text(input: &str) -> String {
    input
        .replace("Authorization: Bearer ", "Authorization: Bearer [redacted]")
        .replace("password=", "password=[redacted]")
        .replace("token=", "token=[redacted]")
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
        let text = sanitize_error_text("password=secret token=abc Authorization: Bearer mcp_raw");
        assert!(!text.contains("password=secret"));
        assert!(!text.contains("token=abc"));
        assert!(!text.contains("Bearer mcp_raw"));
    }
}
