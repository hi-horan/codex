use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const STATSIG_OTLP_HTTP_ENDPOINT: &str = "https://ab.chatgpt.com/otlp/v1/metrics";
pub(crate) const STATSIG_API_KEY_HEADER: &str = "statsig-api-key";
pub(crate) const STATSIG_API_KEY: &str = "client-MkRuleRQBd6qakfnDYqJVR9JuXcY57Ljly3vi5JVUIO";

pub(crate) fn resolve_exporter(exporter: &OtelExporter) -> OtelExporter {
    match exporter {
        OtelExporter::Statsig => {
            // Keep the built-in Statsig default off in debug builds so
            // incremental local development and test runs do not emit
            // best-effort OTEL traffic unless a test or binary opts into an
            // explicit exporter configuration.
            if cfg!(debug_assertions) {
                return OtelExporter::None;
            }

            OtelExporter::OtlpHttp {
                endpoint: STATSIG_OTLP_HTTP_ENDPOINT.to_string(),
                headers: HashMap::from([(
                    STATSIG_API_KEY_HEADER.to_string(),
                    STATSIG_API_KEY.to_string(),
                )]),
                protocol: OtelHttpProtocol::Json,
                tls: None,
            }
        }
        OtelExporter::Langfuse {
            endpoint,
            public_key,
            secret_key,
            protocol,
            tls,
        } => crate::langfuse::resolve_exporter(
            endpoint.clone(),
            public_key.clone(),
            secret_key.clone(),
            protocol.clone(),
            tls.clone(),
        ),
        _ => exporter.clone(),
    }
}

pub(crate) fn exporter_uses_langfuse(exporter: &OtelExporter) -> bool {
    match exporter {
        OtelExporter::Langfuse { .. } => true,
        OtelExporter::OtlpHttp {
            endpoint, headers, ..
        } => endpoint_looks_like_langfuse(endpoint) || headers_include_langfuse_ingestion(headers),
        OtelExporter::None | OtelExporter::Statsig | OtelExporter::OtlpGrpc { .. } => false,
    }
}

fn endpoint_looks_like_langfuse(endpoint: &str) -> bool {
    let endpoint = endpoint.to_ascii_lowercase();
    endpoint.contains("/api/public/otel") || endpoint.contains("langfuse")
}

fn headers_include_langfuse_ingestion(headers: &HashMap<String, String>) -> bool {
    headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("x-langfuse-ingestion-version") && value == "4"
    })
}

/// Validates configured span attributes before they are attached to exported spans.
pub fn validate_span_attributes(attributes: &BTreeMap<String, String>) -> std::io::Result<()> {
    if attributes.keys().any(String::is_empty) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "configured span attribute key must not be empty",
        ));
    }

    Ok(())
}

#[derive(Clone, Debug)]
pub struct OtelSettings {
    pub environment: String,
    pub service_name: String,
    pub service_version: String,
    pub codex_home: PathBuf,
    pub exporter: OtelExporter,
    pub trace_exporter: OtelExporter,
    pub metrics_exporter: OtelExporter,
    pub runtime_metrics: bool,
    pub span_attributes: BTreeMap<String, String>,
    pub tracestate: BTreeMap<String, BTreeMap<String, String>>,
}

/// Resolved Statsig metrics settings that another process can use to recreate
/// the built-in metrics exporter configuration without receiving generic
/// exporter credentials in-process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsigMetricsSettings {
    pub environment: String,
}

#[derive(Clone, Debug)]
pub enum OtelHttpProtocol {
    /// HTTP protocol with binary protobuf
    Binary,
    /// HTTP protocol with JSON payload
    Json,
}

#[derive(Clone, Debug, Default)]
pub struct OtelTlsConfig {
    pub ca_certificate: Option<AbsolutePathBuf>,
    pub client_certificate: Option<AbsolutePathBuf>,
    pub client_private_key: Option<AbsolutePathBuf>,
}

#[derive(Clone, Debug)]
pub enum OtelExporter {
    None,
    /// Statsig metrics ingestion exporter using Codex-internal defaults.
    ///
    /// This is intended for metrics only.
    Statsig,
    OtlpGrpc {
        endpoint: String,
        headers: HashMap<String, String>,
        tls: Option<OtelTlsConfig>,
    },
    OtlpHttp {
        endpoint: String,
        headers: HashMap<String, String>,
        protocol: OtelHttpProtocol,
        tls: Option<OtelTlsConfig>,
    },
    /// Langfuse OTLP/HTTP trace exporter.
    ///
    /// This is intended for trace export. It resolves to an OTLP/HTTP exporter
    /// with Langfuse Basic Auth and ingestion-version headers.
    Langfuse {
        endpoint: String,
        public_key: String,
        secret_key: String,
        protocol: OtelHttpProtocol,
        tls: Option<OtelTlsConfig>,
    },
}

#[cfg(test)]
mod tests {
    use super::OtelExporter;
    use super::exporter_uses_langfuse;
    use super::resolve_exporter;

    #[test]
    fn statsig_default_metrics_exporter_is_disabled_in_debug_builds() {
        assert!(matches!(
            resolve_exporter(&OtelExporter::Statsig),
            OtelExporter::None
        ));
    }

    #[test]
    fn langfuse_exporter_resolves_to_otlp_http_with_required_headers() {
        let resolved = resolve_exporter(&OtelExporter::Langfuse {
            endpoint: "https://example.com/api/public/otel/v1/traces".to_string(),
            public_key: "pk-lf-test".to_string(),
            secret_key: "sk-lf-test".to_string(),
            protocol: super::OtelHttpProtocol::Json,
            tls: None,
        });

        let OtelExporter::OtlpHttp {
            endpoint,
            headers,
            protocol,
            tls,
        } = resolved
        else {
            panic!("expected langfuse exporter to resolve to OTLP HTTP");
        };

        assert_eq!(endpoint, "https://example.com/api/public/otel/v1/traces");
        assert_eq!(
            headers.get("x-langfuse-ingestion-version"),
            Some(&"4".to_string())
        );
        assert_eq!(
            headers.get("Authorization"),
            Some(&"Basic cGstbGYtdGVzdDpzay1sZi10ZXN0".to_string())
        );
        assert!(matches!(protocol, super::OtelHttpProtocol::Json));
        assert!(tls.is_none());
    }

    #[test]
    fn otlp_http_langfuse_endpoint_enables_langfuse_observations() {
        assert!(exporter_uses_langfuse(&OtelExporter::OtlpHttp {
            endpoint: "https://us.cloud.langfuse.com/api/public/otel/v1/traces".to_string(),
            headers: std::collections::HashMap::new(),
            protocol: super::OtelHttpProtocol::Json,
            tls: None,
        }));
    }

    #[test]
    fn otlp_http_langfuse_header_enables_langfuse_observations() {
        assert!(exporter_uses_langfuse(&OtelExporter::OtlpHttp {
            endpoint: "https://otel.example.com/v1/traces".to_string(),
            headers: std::collections::HashMap::from([(
                "x-langfuse-ingestion-version".to_string(),
                "4".to_string(),
            )]),
            protocol: super::OtelHttpProtocol::Json,
            tls: None,
        }));
    }

    #[test]
    fn plain_otlp_http_endpoint_does_not_enable_langfuse_observations() {
        assert!(!exporter_uses_langfuse(&OtelExporter::OtlpHttp {
            endpoint: "https://otel.example.com/v1/traces".to_string(),
            headers: std::collections::HashMap::new(),
            protocol: super::OtelHttpProtocol::Json,
            tls: None,
        }));
    }
}
