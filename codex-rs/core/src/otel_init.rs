use crate::config::Config;
use codex_config::types::OtelExporterKind as Kind;
use codex_config::types::OtelHttpProtocol as Protocol;
use codex_config::types::OtelLangfuseConfig;
use codex_config::types::OtelTlsConfig;
use codex_features::Feature;
use codex_login::default_client::originator;
use codex_otel::OtelExporter;
use codex_otel::OtelHttpProtocol;
use codex_otel::OtelProvider;
use codex_otel::OtelSettings;
use codex_otel::OtelTlsConfig as OtelTlsSettings;
use std::error::Error;
use std::io;

struct LangfuseExporterConfig<'a> {
    endpoint: Option<&'a str>,
    public_key: Option<&'a str>,
    secret_key: Option<&'a str>,
    public_key_env_var: Option<&'a str>,
    secret_key_env_var: Option<&'a str>,
    protocol: Option<&'a Protocol>,
    tls: Option<&'a OtelTlsConfig>,
    config_path: &'static str,
}

/// Build an OpenTelemetry provider from the app Config.
///
/// Returns `None` when OTEL export is disabled.
pub fn build_provider(
    config: &Config,
    service_version: &str,
    service_name_override: Option<&str>,
    default_analytics_enabled: bool,
) -> Result<Option<OtelProvider>, Box<dyn Error>> {
    let to_otel_exporter = |kind: &Kind| -> Result<OtelExporter, Box<dyn Error>> {
        let exporter = match kind {
            Kind::None => OtelExporter::None,
            Kind::Statsig => OtelExporter::Statsig,
            Kind::Langfuse {
                endpoint,
                public_key,
                secret_key,
                public_key_env_var,
                secret_key_env_var,
                protocol,
                tls,
            } => langfuse_exporter_from_config(LangfuseExporterConfig {
                endpoint: endpoint.as_deref(),
                public_key: public_key.as_deref(),
                secret_key: secret_key.as_deref(),
                public_key_env_var: public_key_env_var.as_deref(),
                secret_key_env_var: secret_key_env_var.as_deref(),
                protocol: protocol.as_ref(),
                tls: tls.as_ref(),
                config_path: "otel.trace_exporter.langfuse",
            })?,
            Kind::OtlpHttp {
                endpoint,
                headers,
                protocol,
                tls,
            } => {
                let protocol = match protocol {
                    Protocol::Json => OtelHttpProtocol::Json,
                    Protocol::Binary => OtelHttpProtocol::Binary,
                };

                OtelExporter::OtlpHttp {
                    endpoint: endpoint.clone(),
                    headers: headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    protocol,
                    tls: tls.as_ref().map(|config| OtelTlsSettings {
                        ca_certificate: config.ca_certificate.clone(),
                        client_certificate: config.client_certificate.clone(),
                        client_private_key: config.client_private_key.clone(),
                    }),
                }
            }
            Kind::OtlpGrpc {
                endpoint,
                headers,
                tls,
            } => OtelExporter::OtlpGrpc {
                endpoint: endpoint.clone(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                tls: tls.as_ref().map(|config| OtelTlsSettings {
                    ca_certificate: config.ca_certificate.clone(),
                    client_certificate: config.client_certificate.clone(),
                    client_private_key: config.client_private_key.clone(),
                }),
            },
        };
        Ok(exporter)
    };

    let exporter = to_otel_exporter(&config.otel.exporter)?;
    let trace_exporter = if let Some(langfuse) = &config.otel.langfuse
        && langfuse.enabled
    {
        langfuse_exporter_from_cli_config(langfuse)?
    } else {
        to_otel_exporter(&config.otel.trace_exporter)?
    };
    let metrics_exporter = if config
        .analytics_enabled
        .unwrap_or(default_analytics_enabled)
    {
        to_otel_exporter(&config.otel.metrics_exporter)?
    } else {
        OtelExporter::None
    };

    let originator = originator();
    let service_name = service_name_override.unwrap_or(originator.value.as_str());
    let runtime_metrics = config.features.enabled(Feature::RuntimeMetrics);

    OtelProvider::from(&OtelSettings {
        service_name: service_name.to_string(),
        service_version: service_version.to_string(),
        codex_home: config.codex_home.to_path_buf(),
        environment: config.otel.environment.to_string(),
        exporter,
        trace_exporter,
        metrics_exporter,
        runtime_metrics,
        span_attributes: config.otel.span_attributes.clone(),
        tracestate: config.otel.tracestate.clone(),
    })
}

fn langfuse_exporter_from_cli_config(
    config: &OtelLangfuseConfig,
) -> Result<OtelExporter, Box<dyn Error>> {
    langfuse_exporter_from_config(LangfuseExporterConfig {
        endpoint: config.endpoint.as_deref(),
        public_key: config.public_key.as_deref(),
        secret_key: config.secret_key.as_deref(),
        public_key_env_var: config.public_key_env_var.as_deref(),
        secret_key_env_var: config.secret_key_env_var.as_deref(),
        protocol: config.protocol.as_ref(),
        tls: config.tls.as_ref(),
        config_path: "otel.langfuse",
    })
}

fn langfuse_exporter_from_config(
    config: LangfuseExporterConfig<'_>,
) -> Result<OtelExporter, Box<dyn Error>> {
    Ok(OtelExporter::Langfuse {
        endpoint: config
            .endpoint
            .map(str::to_string)
            .unwrap_or_else(|| codex_otel::DEFAULT_LANGFUSE_OTLP_TRACES_ENDPOINT.to_string()),
        public_key: resolve_langfuse_credential(
            config.config_path,
            "public_key",
            config.public_key,
            config.public_key_env_var,
        )?,
        secret_key: resolve_langfuse_credential(
            config.config_path,
            "secret_key",
            config.secret_key,
            config.secret_key_env_var,
        )?,
        protocol: config
            .protocol
            .map_or(OtelHttpProtocol::Json, |protocol| match protocol {
                Protocol::Json => OtelHttpProtocol::Json,
                Protocol::Binary => OtelHttpProtocol::Binary,
            }),
        tls: config.tls.map(|tls| OtelTlsSettings {
            ca_certificate: tls.ca_certificate.clone(),
            client_certificate: tls.client_certificate.clone(),
            client_private_key: tls.client_private_key.clone(),
        }),
    })
}

fn resolve_langfuse_credential(
    config_path: &str,
    field_name: &str,
    value: Option<&str>,
    env_var: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    if let Some(value) = value
        && !value.is_empty()
    {
        return Ok(value.to_string());
    }

    if let Some(env_var) = env_var
        && !env_var.is_empty()
    {
        return std::env::var(env_var).map_err(|err| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("failed to read {config_path}.{field_name} from `{env_var}`: {err}"),
            )) as Box<dyn Error>
        });
    }

    Err(Box::new(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{config_path} requires `{field_name}` or `{field_name}_env_var`",),
    )))
}

/// Filter predicate for exporting only Codex-owned events via OTEL.
/// Keeps events that originated from codex_otel module
pub fn codex_export_filter(meta: &tracing::Metadata<'_>) -> bool {
    meta.target().starts_with("codex_otel")
}
