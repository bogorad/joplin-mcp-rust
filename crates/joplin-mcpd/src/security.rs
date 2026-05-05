use crate::config::ServerConfig;
use anyhow::bail;
use axum::http::{HeaderMap, Method, StatusCode, header};
use std::fmt;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointClass {
    Mcp,
    Api,
    LoginGet,
    LoginPost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPolicyError {
    DisallowedOrigin,
    DisallowedReferer,
    MissingNonBrowserJsonSignal,
}

impl OriginPolicyError {
    pub fn status(self) -> StatusCode {
        match self {
            Self::DisallowedOrigin
            | Self::DisallowedReferer
            | Self::MissingNonBrowserJsonSignal => StatusCode::FORBIDDEN,
        }
    }
}

impl fmt::Display for OriginPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisallowedOrigin => formatter.write_str("origin is not allowed"),
            Self::DisallowedReferer => formatter.write_str("referer is not allowed"),
            Self::MissingNonBrowserJsonSignal => {
                formatter.write_str("absent origin requires non-browser JSON request")
            }
        }
    }
}

pub fn validate_server_policy(config: &ServerConfig) -> anyhow::Result<()> {
    if config.allowed_origins.is_empty() {
        bail!("server.allowed_origins must not be empty");
    }

    for origin in &config.allowed_origins {
        if normalize_origin_header_value(origin).is_none() {
            bail!("server.allowed_origins entries must be http(s) origins");
        }
    }

    Ok(())
}

pub fn endpoint_class(method: &Method, path: &str) -> Option<EndpointClass> {
    match (method, path) {
        (_, "/mcp") => Some(EndpointClass::Mcp),
        (_, path) if path.starts_with("/api/") => Some(EndpointClass::Api),
        (&Method::GET, "/login") => Some(EndpointClass::LoginGet),
        (&Method::POST, "/login") => Some(EndpointClass::LoginPost),
        _ => None,
    }
}

pub fn validate_origin_policy(
    class: EndpointClass,
    headers: &HeaderMap,
    allowed_origins: &[String],
) -> Result<(), OriginPolicyError> {
    match class {
        EndpointClass::Mcp | EndpointClass::Api => validate_json_endpoint(headers, allowed_origins),
        EndpointClass::LoginGet => validate_optional_origin(headers, allowed_origins),
        EndpointClass::LoginPost => validate_login_post(headers, allowed_origins),
    }
}

fn validate_json_endpoint(
    headers: &HeaderMap,
    allowed_origins: &[String],
) -> Result<(), OriginPolicyError> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        return allowed_origin_header(origin.to_str().ok(), allowed_origins)
            .then_some(())
            .ok_or(OriginPolicyError::DisallowedOrigin);
    }

    if has_bearer_authorization(headers) && has_json_content_type(headers) {
        Ok(())
    } else {
        Err(OriginPolicyError::MissingNonBrowserJsonSignal)
    }
}

fn validate_optional_origin(
    headers: &HeaderMap,
    allowed_origins: &[String],
) -> Result<(), OriginPolicyError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };

    allowed_origin_header(origin.to_str().ok(), allowed_origins)
        .then_some(())
        .ok_or(OriginPolicyError::DisallowedOrigin)
}

fn validate_login_post(
    headers: &HeaderMap,
    allowed_origins: &[String],
) -> Result<(), OriginPolicyError> {
    let origin = headers.get(header::ORIGIN);
    let referer = headers.get(header::REFERER);

    if let Some(origin) = origin
        && !allowed_origin_header(origin.to_str().ok(), allowed_origins)
    {
        return Err(OriginPolicyError::DisallowedOrigin);
    }

    if let Some(referer) = referer
        && !allowed_referer_header(referer.to_str().ok(), allowed_origins)
    {
        return Err(OriginPolicyError::DisallowedReferer);
    }

    if origin.is_none() && referer.is_none() {
        return Err(OriginPolicyError::DisallowedOrigin);
    }

    Ok(())
}

fn has_bearer_authorization(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer "))
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
}

fn allowed_origin_header(origin: Option<&str>, allowed_origins: &[String]) -> bool {
    let Some(origin) = origin.and_then(normalize_origin_header_value) else {
        return false;
    };
    allowed_origins
        .iter()
        .filter_map(|allowed| normalize_origin_header_value(allowed))
        .any(|allowed| allowed == origin)
}

fn allowed_referer_header(referer: Option<&str>, allowed_origins: &[String]) -> bool {
    let Some(origin) = referer.and_then(origin_from_url_value) else {
        return false;
    };
    allowed_origins
        .iter()
        .filter_map(|allowed| normalize_origin_header_value(allowed))
        .any(|allowed| allowed == origin)
}

fn normalize_origin_header_value(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    origin_from_url(&url)
}

fn origin_from_url_value(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    origin_from_url(&url)
}

fn origin_from_url(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, header};

    fn allowed() -> Vec<String> {
        vec!["https://joplin-mcp.lan".to_string()]
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer mcp_test"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers
    }

    #[test]
    fn origin_policy_allows_browser_origin_for_mcp_and_api() {
        let mut headers = json_headers();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://joplin-mcp.lan"),
        );

        validate_origin_policy(EndpointClass::Mcp, &headers, &allowed()).expect("allowed mcp");
        validate_origin_policy(EndpointClass::Api, &headers, &allowed()).expect("allowed api");
    }

    #[test]
    fn origin_policy_rejects_unexpected_origin_for_mcp() {
        let mut headers = json_headers();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.test"),
        );

        assert_eq!(
            validate_origin_policy(EndpointClass::Mcp, &headers, &allowed()),
            Err(OriginPolicyError::DisallowedOrigin)
        );
    }

    #[test]
    fn origin_policy_allows_absent_origin_for_non_browser_json() {
        validate_origin_policy(EndpointClass::Mcp, &json_headers(), &allowed())
            .expect("non-browser JSON accepted");
    }

    #[test]
    fn origin_policy_rejects_absent_origin_without_json_and_bearer_signals() {
        let headers = HeaderMap::new();

        assert_eq!(
            validate_origin_policy(EndpointClass::Api, &headers, &allowed()),
            Err(OriginPolicyError::MissingNonBrowserJsonSignal)
        );
    }

    #[test]
    fn login_get_allows_absent_origin_and_rejects_unexpected_origin() {
        validate_origin_policy(EndpointClass::LoginGet, &HeaderMap::new(), &allowed())
            .expect("navigation without origin accepted");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.test"),
        );
        assert_eq!(
            validate_origin_policy(EndpointClass::LoginGet, &headers, &allowed()),
            Err(OriginPolicyError::DisallowedOrigin)
        );
    }

    #[test]
    fn login_post_validates_origin_and_referer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://joplin-mcp.lan"),
        );
        headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://joplin-mcp.lan/login"),
        );
        validate_origin_policy(EndpointClass::LoginPost, &headers, &allowed())
            .expect("allowed origin and referer accepted");

        headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://evil.test/login"),
        );
        assert_eq!(
            validate_origin_policy(EndpointClass::LoginPost, &headers, &allowed()),
            Err(OriginPolicyError::DisallowedReferer)
        );
    }

    #[test]
    fn login_post_requires_origin_or_referer() {
        assert_eq!(
            validate_origin_policy(EndpointClass::LoginPost, &HeaderMap::new(), &allowed()),
            Err(OriginPolicyError::DisallowedOrigin)
        );
    }

    #[test]
    fn login_post_allows_origin_or_referer() {
        let mut origin_headers = HeaderMap::new();
        origin_headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://joplin-mcp.lan"),
        );
        validate_origin_policy(EndpointClass::LoginPost, &origin_headers, &allowed())
            .expect("allowed origin accepted");

        let mut referer_headers = HeaderMap::new();
        referer_headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://joplin-mcp.lan/login"),
        );
        validate_origin_policy(EndpointClass::LoginPost, &referer_headers, &allowed())
            .expect("allowed referer accepted");
    }

    #[test]
    fn endpoint_classes_cover_browser_reachable_paths() {
        assert_eq!(
            endpoint_class(&Method::POST, "/mcp"),
            Some(EndpointClass::Mcp)
        );
        assert_eq!(
            endpoint_class(&Method::GET, "/mcp"),
            Some(EndpointClass::Mcp)
        );
        assert_eq!(
            endpoint_class(&Method::OPTIONS, "/mcp"),
            Some(EndpointClass::Mcp)
        );
        assert_eq!(
            endpoint_class(&Method::DELETE, "/mcp"),
            Some(EndpointClass::Mcp)
        );
        assert_eq!(
            endpoint_class(&Method::POST, "/api/bootstrap/login"),
            Some(EndpointClass::Api)
        );
        assert_eq!(
            endpoint_class(&Method::GET, "/login"),
            Some(EndpointClass::LoginGet)
        );
        assert_eq!(
            endpoint_class(&Method::POST, "/login"),
            Some(EndpointClass::LoginPost)
        );
        assert_eq!(endpoint_class(&Method::GET, "/healthz"), None);
    }
}
