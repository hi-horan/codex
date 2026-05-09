use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine as _;
use codex_protocol::protocol::TokenUsage;
use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::Span as _;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::Span;
use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::SpanProcessor;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::OtelExporter;
use crate::config::OtelHttpProtocol;
use crate::config::OtelTlsConfig;
use crate::events::session_telemetry::SessionTelemetryMetadata;

pub const DEFAULT_LANGFUSE_OTLP_TRACES_ENDPOINT: &str =
    "https://cloud.langfuse.com/api/public/otel/v1/traces";

const LANGFUSE_AUTH_HEADER: &str = "Authorization";
const LANGFUSE_INGESTION_VERSION_HEADER: &str = "x-langfuse-ingestion-version";
const LANGFUSE_INGESTION_VERSION: &str = "4";

const LANGFUSE_OBSERVATION_TYPE: &str = "langfuse.observation.type";
const LANGFUSE_OBSERVATION_INPUT: &str = "langfuse.observation.input";
const LANGFUSE_OBSERVATION_OUTPUT: &str = "langfuse.observation.output";
const LANGFUSE_OBSERVATION_LEVEL: &str = "langfuse.observation.level";
const LANGFUSE_OBSERVATION_STATUS_MESSAGE: &str = "langfuse.observation.status_message";
const LANGFUSE_OBSERVATION_MODEL_NAME: &str = "langfuse.observation.model.name";
const LANGFUSE_OBSERVATION_MODEL_PARAMETERS: &str = "langfuse.observation.model.parameters";
const LANGFUSE_OBSERVATION_USAGE_DETAILS: &str = "langfuse.observation.usage_details";
const LANGFUSE_OBSERVATION_METADATA_CODEX_KIND: &str =
    "langfuse.observation.metadata.codex_observation_kind";
const LANGFUSE_OBSERVATION_METADATA_CODEX: &str =
    "langfuse.observation.metadata.codex_observation_metadata";

static LANGFUSE_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(enabled: bool) {
    LANGFUSE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    LANGFUSE_ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn resolve_exporter(
    endpoint: String,
    public_key: String,
    secret_key: String,
    protocol: OtelHttpProtocol,
    tls: Option<OtelTlsConfig>,
) -> OtelExporter {
    let auth =
        base64::engine::general_purpose::STANDARD.encode(format!("{public_key}:{secret_key}"));
    OtelExporter::OtlpHttp {
        endpoint,
        headers: HashMap::from([
            (LANGFUSE_AUTH_HEADER.to_string(), format!("Basic {auth}")),
            (
                LANGFUSE_INGESTION_VERSION_HEADER.to_string(),
                LANGFUSE_INGESTION_VERSION.to_string(),
            ),
        ]),
        protocol,
        tls,
    }
}

/// Copies Langfuse baggage entries onto every span when the span starts.
#[derive(Debug, Default)]
pub(crate) struct BaggageSpanAttributesProcessor;

impl SpanProcessor for BaggageSpanAttributesProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        for (key, (value, _metadata)) in cx.baggage().iter() {
            let key = key.as_str();
            if is_langfuse_baggage_attribute(key) {
                span.set_attribute(KeyValue::new(key.to_string(), value.to_string()));
            }
        }
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

fn is_langfuse_baggage_attribute(key: &str) -> bool {
    matches!(
        key,
        "langfuse.user.id"
            | "user.id"
            | "langfuse.session.id"
            | "session.id"
            | "langfuse.trace.name"
            | "langfuse.version"
            | "langfuse.release"
            | "langfuse.environment"
            | LANGFUSE_OBSERVATION_TYPE
    ) || key.starts_with("langfuse.trace.metadata.")
}

pub(crate) fn set_session_parent_context(
    metadata: &SessionTelemetryMetadata,
    span: &tracing::Span,
    trace_name: &str,
) {
    if !enabled() {
        return;
    }

    let baggage = session_baggage(metadata, trace_name);
    apply_baggage_to_tracing_span(span, &baggage);
    let cx = Context::current_with_baggage(baggage);
    let _ = span.set_parent(cx);
}

fn session_baggage(metadata: &SessionTelemetryMetadata, trace_name: &str) -> Vec<KeyValue> {
    let conversation_id = metadata.conversation_id.to_string();
    let mut baggage = vec![
        KeyValue::new("langfuse.session.id", conversation_id.clone()),
        KeyValue::new("session.id", conversation_id.clone()),
        KeyValue::new("langfuse.trace.name", trace_name.to_string()),
        KeyValue::new("langfuse.version", metadata.app_version),
        KeyValue::new("langfuse.release", metadata.app_version),
        KeyValue::new(LANGFUSE_OBSERVATION_TYPE, "span"),
        KeyValue::new("langfuse.trace.metadata.conversation_id", conversation_id),
        KeyValue::new(
            "langfuse.trace.metadata.originator",
            metadata.originator.clone(),
        ),
        KeyValue::new(
            "langfuse.trace.metadata.session_source",
            metadata.session_source.clone(),
        ),
        KeyValue::new("langfuse.trace.metadata.model", metadata.model.clone()),
        KeyValue::new("langfuse.trace.metadata.slug", metadata.slug.clone()),
        KeyValue::new(
            "langfuse.trace.metadata.terminal_type",
            metadata.terminal_type.clone(),
        ),
    ];

    if let Some(account_id) = metadata.account_id.as_deref() {
        baggage.push(KeyValue::new("langfuse.user.id", account_id.to_string()));
        baggage.push(KeyValue::new("user.id", account_id.to_string()));
        baggage.push(KeyValue::new(
            "langfuse.trace.metadata.account_id",
            account_id.to_string(),
        ));
    }
    if let Some(account_email) = metadata.account_email.as_deref() {
        baggage.push(KeyValue::new(
            "langfuse.trace.metadata.account_email",
            account_email.to_string(),
        ));
    }
    if let Some(auth_mode) = metadata.auth_mode.as_deref() {
        baggage.push(KeyValue::new(
            "langfuse.trace.metadata.auth_mode",
            auth_mode.to_string(),
        ));
    }

    baggage
}

fn apply_baggage_to_tracing_span(span: &tracing::Span, baggage: &[KeyValue]) {
    for item in baggage {
        let key = item.key.as_str();
        if is_langfuse_baggage_attribute(key) {
            span.set_attribute(key.to_string(), item.value.to_string());
        }
    }
}

pub(crate) fn record_generation_started(
    span: &tracing::Span,
    input: Value,
    model_name: &str,
    provider_name: &str,
    model_parameters: Value,
) {
    if !enabled() {
        return;
    }

    span.set_attribute(LANGFUSE_OBSERVATION_TYPE, "generation");
    span.set_attribute(LANGFUSE_OBSERVATION_INPUT, input.to_string());
    span.set_attribute(LANGFUSE_OBSERVATION_MODEL_NAME, model_name.to_string());
    span.set_attribute(
        LANGFUSE_OBSERVATION_MODEL_PARAMETERS,
        model_parameters.to_string(),
    );
    span.set_attribute("gen_ai.system", provider_name.to_string());
    span.set_attribute("gen_ai.request.model", model_name.to_string());
}

pub(crate) fn record_generation_completed(
    span: &tracing::Span,
    output: Value,
    token_usage: Option<&TokenUsage>,
) {
    if !enabled() {
        return;
    }

    span.set_attribute(LANGFUSE_OBSERVATION_OUTPUT, output.to_string());
    span.set_attribute(LANGFUSE_OBSERVATION_LEVEL, "DEFAULT");

    if let Some(token_usage) = token_usage {
        span.set_attribute("gen_ai.usage.input_tokens", token_usage.input_tokens);
        span.set_attribute(
            "gen_ai.usage.cache_read.input_tokens",
            token_usage.cached_input_tokens,
        );
        span.set_attribute("gen_ai.usage.output_tokens", token_usage.output_tokens);
        span.set_attribute(
            LANGFUSE_OBSERVATION_USAGE_DETAILS,
            serde_json::json!({
                "input": token_usage.input_tokens,
                "cache_read_input": token_usage.cached_input_tokens,
                "output": token_usage.output_tokens,
                "reasoning_output": token_usage.reasoning_output_tokens,
                "total": token_usage.total_tokens,
            })
            .to_string(),
        );
    }
}

pub(crate) fn record_generation_failed(span: &tracing::Span, error: &str) {
    if !enabled() {
        return;
    }

    span.set_attribute(LANGFUSE_OBSERVATION_LEVEL, "ERROR");
    span.set_attribute(LANGFUSE_OBSERVATION_STATUS_MESSAGE, error.to_string());
}

pub(crate) fn record_generation_metadata(span: &tracing::Span, metadata: Value) {
    if !enabled() {
        return;
    }

    record_observation_metadata(span, "sampling", metadata);
}

pub(crate) fn record_compaction_generation_started(
    span: &tracing::Span,
    input: Value,
    model_name: &str,
    provider_name: &str,
    model_parameters: Value,
    metadata: Value,
) {
    if !enabled() {
        return;
    }

    record_generation_started(span, input, model_name, provider_name, model_parameters);
    record_observation_metadata(span, "context_compaction", metadata);
}

pub(crate) fn record_compaction_generation_completed(
    span: &tracing::Span,
    output: Value,
    token_usage: Option<&TokenUsage>,
) {
    record_generation_completed(span, output, token_usage);
}

pub(crate) fn record_compaction_installed(
    span: &tracing::Span,
    input: Value,
    output: Value,
    metadata: Value,
) {
    if !enabled() {
        return;
    }

    span.set_attribute(LANGFUSE_OBSERVATION_TYPE, "span");
    span.set_attribute(LANGFUSE_OBSERVATION_INPUT, input.to_string());
    span.set_attribute(LANGFUSE_OBSERVATION_OUTPUT, output.to_string());
    span.set_attribute(LANGFUSE_OBSERVATION_LEVEL, "DEFAULT");
    record_observation_metadata(span, "context_compaction", metadata);
}

pub(crate) fn record_memory_summarize_generation_started(
    span: &tracing::Span,
    input: Value,
    model_name: &str,
    provider_name: &str,
    model_parameters: Value,
    metadata: Value,
) {
    if !enabled() {
        return;
    }

    record_generation_started(span, input, model_name, provider_name, model_parameters);
    record_observation_metadata(span, "memory_summarize", metadata);
}

pub(crate) fn record_realtime_generation_started(
    span: &tracing::Span,
    input: Value,
    model_name: &str,
    provider_name: &str,
    model_parameters: Value,
    metadata: Value,
) {
    if !enabled() {
        return;
    }

    record_generation_started(span, input, model_name, provider_name, model_parameters);
    record_observation_metadata(span, "realtime", metadata);
}

fn record_observation_metadata(span: &tracing::Span, kind: &str, metadata: Value) {
    span.set_attribute(LANGFUSE_OBSERVATION_METADATA_CODEX_KIND, kind.to_string());
    span.set_attribute(LANGFUSE_OBSERVATION_METADATA_CODEX, metadata.to_string());
}

pub(crate) struct ToolResultObservation<'a> {
    pub(crate) tool_name: &'a str,
    pub(crate) call_id: Option<&'a str>,
    pub(crate) arguments: Option<&'a str>,
    pub(crate) output: &'a str,
    pub(crate) success: bool,
    pub(crate) mcp_server: Option<&'a str>,
    pub(crate) mcp_server_origin: Option<&'a str>,
}

pub(crate) fn record_tool_result(span: &tracing::Span, observation: ToolResultObservation<'_>) {
    if !enabled() {
        return;
    }

    span.set_attribute(LANGFUSE_OBSERVATION_TYPE, "span");
    span.set_attribute(
        LANGFUSE_OBSERVATION_INPUT,
        serde_json::json!({
            "tool_name": observation.tool_name,
            "call_id": observation.call_id,
            "arguments": observation.arguments,
        })
        .to_string(),
    );
    span.set_attribute(
        LANGFUSE_OBSERVATION_OUTPUT,
        serde_json::json!({
            "success": observation.success,
            "output": observation.output,
        })
        .to_string(),
    );
    span.set_attribute(
        "langfuse.observation.metadata.tool_name",
        observation.tool_name.to_string(),
    );
    if let Some(call_id) = observation.call_id {
        span.set_attribute("langfuse.observation.metadata.call_id", call_id.to_string());
    }
    if let Some(mcp_server) = observation.mcp_server {
        span.set_attribute(
            "langfuse.observation.metadata.mcp_server",
            mcp_server.to_string(),
        );
    }
    if let Some(mcp_server_origin) = observation.mcp_server_origin {
        span.set_attribute(
            "langfuse.observation.metadata.mcp_server_origin",
            mcp_server_origin.to_string(),
        );
    }
    if !observation.success {
        span.set_attribute(LANGFUSE_OBSERVATION_LEVEL, "ERROR");
        span.set_attribute(
            LANGFUSE_OBSERVATION_STATUS_MESSAGE,
            observation.output.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use codex_protocol::ThreadId;
    use codex_protocol::protocol::TokenUsage;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use pretty_assertions::assert_eq;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    static LANGFUSE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn session_parent_context_propagates_langfuse_baggage_to_descendant_spans() {
        let _guard = LANGFUSE_TEST_LOCK.lock().expect("lock langfuse test");
        let previously_enabled = enabled();
        set_enabled(true);
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .with_span_processor(BaggageSpanAttributesProcessor)
            .build();
        let tracer = tracer_provider.tracer("langfuse-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let conversation_id = ThreadId::new();

        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let metadata = SessionTelemetryMetadata {
                conversation_id,
                auth_mode: Some("api_key".to_string()),
                auth_env: Default::default(),
                account_id: Some("account-123".to_string()),
                account_email: Some("engineer@example.com".to_string()),
                originator: "codex_exec".to_string(),
                service_name: None,
                session_source: "cli".to_string(),
                model: "gpt-5.1".to_string(),
                slug: "gpt-5.1".to_string(),
                log_user_prompts: true,
                app_version: "0.0.0-test",
                terminal_type: "tty".to_string(),
            };
            let root = tracing::info_span!("root");
            set_session_parent_context(&metadata, &root, "codex.turn");
            let _root_guard = root.enter();
            let child = tracing::info_span!("child");
            let _child_guard = child.enter();
        });

        tracer_provider.force_flush().expect("flush spans");
        let spans = span_exporter.get_finished_spans().expect("span export");
        assert_eq!(spans.len(), 2);

        let expected_conversation_id = conversation_id.to_string();
        for span in spans {
            let attrs = span
                .attributes
                .iter()
                .map(|attr| (attr.key.as_str().to_string(), attr.value.to_string()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(
                attrs.get("langfuse.session.id").map(String::as_str),
                Some(expected_conversation_id.as_str())
            );
            assert_eq!(
                attrs.get("langfuse.user.id").map(String::as_str),
                Some("account-123")
            );
            assert_eq!(
                attrs.get("langfuse.trace.name").map(String::as_str),
                Some("codex.turn")
            );
            assert_eq!(
                attrs
                    .get("langfuse.trace.metadata.account_email")
                    .map(String::as_str),
                Some("engineer@example.com")
            );
        }
        set_enabled(previously_enabled);
    }

    #[test]
    fn generation_span_records_raw_input_output_and_usage() {
        let _guard = LANGFUSE_TEST_LOCK.lock().expect("lock langfuse test");
        let previously_enabled = enabled();
        set_enabled(true);
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .build();
        let tracer = tracer_provider.tracer("langfuse-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let generation = tracing::info_span!("generation");
            let _generation_guard = generation.enter();

            record_generation_started(
                &generation,
                serde_json::json!({"prompt": "raw user prompt"}),
                "gpt-5.1",
                "openai",
                serde_json::json!({"reasoning_effort": "high"}),
            );
            record_generation_completed(
                &generation,
                serde_json::json!({"message": "raw assistant output"}),
                Some(&TokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: 3,
                    output_tokens: 7,
                    reasoning_output_tokens: 2,
                    total_tokens: 17,
                }),
            );
        });

        tracer_provider.force_flush().expect("flush spans");
        let spans = span_exporter.get_finished_spans().expect("span export");
        assert_eq!(spans.len(), 1);
        let attrs = spans[0]
            .attributes
            .iter()
            .map(|attr| (attr.key.as_str().to_string(), attr.value.to_string()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_TYPE).map(String::as_str),
            Some("generation")
        );
        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_INPUT).map(String::as_str),
            Some("{\"prompt\":\"raw user prompt\"}")
        );
        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_OUTPUT).map(String::as_str),
            Some("{\"message\":\"raw assistant output\"}")
        );
        assert_eq!(
            attrs.get("gen_ai.usage.input_tokens").map(String::as_str),
            Some("10")
        );
        set_enabled(previously_enabled);
    }

    #[test]
    fn generation_span_records_codex_sampling_metadata() {
        let _guard = LANGFUSE_TEST_LOCK.lock().expect("lock langfuse test");
        let previously_enabled = enabled();
        set_enabled(true);
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .build();
        let tracer = tracer_provider.tracer("langfuse-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let generation = tracing::info_span!("generation");
            let _generation_guard = generation.enter();

            record_generation_metadata(
                &generation,
                serde_json::json!({
                    "input_compaction_item_count": 1,
                    "input_contains_compaction": true,
                }),
            );
        });

        tracer_provider.force_flush().expect("flush spans");
        let spans = span_exporter.get_finished_spans().expect("span export");
        assert_eq!(spans.len(), 1);
        let attrs = spans[0]
            .attributes
            .iter()
            .map(|attr| (attr.key.as_str().to_string(), attr.value.to_string()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            attrs
                .get(LANGFUSE_OBSERVATION_METADATA_CODEX_KIND)
                .map(String::as_str),
            Some("sampling")
        );
        assert_eq!(
            attrs
                .get(LANGFUSE_OBSERVATION_METADATA_CODEX)
                .map(String::as_str),
            Some("{\"input_compaction_item_count\":1,\"input_contains_compaction\":true}")
        );
        set_enabled(previously_enabled);
    }

    #[test]
    fn compaction_generation_records_trace_payload_shape() {
        let _guard = LANGFUSE_TEST_LOCK.lock().expect("lock langfuse test");
        let previously_enabled = enabled();
        set_enabled(true);
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .build();
        let tracer = tracer_provider.tracer("langfuse-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let compaction = tracing::info_span!("context_compaction");
            let _compaction_guard = compaction.enter();

            record_compaction_generation_started(
                &compaction,
                serde_json::json!({"input": ["history"]}),
                "gpt-5.1",
                "openai",
                serde_json::json!({"reasoning_effort": "high"}),
                serde_json::json!({
                    "compaction_id": "context-compaction-1",
                    "implementation": "responses_compact",
                }),
            );
            record_compaction_generation_completed(
                &compaction,
                serde_json::json!({"output_items": ["summary"]}),
                None,
            );
        });

        tracer_provider.force_flush().expect("flush spans");
        let spans = span_exporter.get_finished_spans().expect("span export");
        assert_eq!(spans.len(), 1);
        let attrs = spans[0]
            .attributes
            .iter()
            .map(|attr| (attr.key.as_str().to_string(), attr.value.to_string()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_TYPE).map(String::as_str),
            Some("generation")
        );
        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_INPUT).map(String::as_str),
            Some("{\"input\":[\"history\"]}")
        );
        assert_eq!(
            attrs.get(LANGFUSE_OBSERVATION_OUTPUT).map(String::as_str),
            Some("{\"output_items\":[\"summary\"]}")
        );
        assert_eq!(
            attrs
                .get(LANGFUSE_OBSERVATION_METADATA_CODEX_KIND)
                .map(String::as_str),
            Some("context_compaction")
        );
        assert_eq!(
            attrs
                .get(LANGFUSE_OBSERVATION_METADATA_CODEX)
                .map(String::as_str),
            Some(
                "{\"compaction_id\":\"context-compaction-1\",\"implementation\":\"responses_compact\"}"
            )
        );
        set_enabled(previously_enabled);
    }
}
