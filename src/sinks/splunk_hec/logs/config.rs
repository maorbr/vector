use std::sync::Arc;

use codecs::TextSerializerConfig;
use futures_util::FutureExt;
use lookup::lookup_v2::OptionalValuePath;
use tower::ServiceBuilder;
use vector_common::sensitive_string::SensitiveString;
use vector_config::configurable_component;
use vector_core::sink::VectorSink;

use super::{encoder::HecLogsEncoder, request_builder::HecLogsRequestBuilder, sink::HecLogsSink};
use crate::sinks::splunk_hec::common::config_timestamp_key;
use crate::{
    codecs::{Encoder, EncodingConfig},
    config::{AcknowledgementsConfig, DataType, GenerateConfig, Input, SinkConfig, SinkContext},
    http::HttpClient,
    sinks::{
        splunk_hec::common::{
            acknowledgements::HecClientAcknowledgementsConfig,
            build_healthcheck, build_http_batch_service, create_client, host_key,
            service::{HecService, HttpRequestBuilder},
            EndpointTarget, SplunkHecDefaultBatchSettings,
        },
        util::{
            http::HttpRetryLogic, BatchConfig, Compression, ServiceBuilderExt, TowerRequestConfig,
        },
        Healthcheck,
    },
    template::Template,
    tls::TlsConfig,
};

/// Configuration for the `splunk_hec_logs` sink.
#[configurable_component(sink("splunk_hec_logs"))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct HecLogsSinkConfig {
    /// Default Splunk HEC token.
    ///
    /// If an event has a token set in its secrets (`splunk_hec_token`), it prevails over the one set here.
    #[serde(alias = "token")]
    pub default_token: SensitiveString,

    /// The base URL of the Splunk instance.
    ///
    /// The scheme (`http` or `https`) must be specified. No path should be included since the paths defined
    /// by the [`Splunk`][splunk] API are used.
    ///
    /// [splunk]: https://docs.splunk.com/Documentation/Splunk/8.0.0/Data/HECRESTendpoints
    #[configurable(metadata(
        docs::examples = "https://http-inputs-hec.splunkcloud.com",
        docs::examples = "https://hec.splunk.com:8088",
        docs::examples = "http://example.com"
    ))]
    #[configurable(validation(format = "uri"))]
    pub endpoint: String,

    /// Overrides the name of the log field used to retrieve the hostname to send to Splunk HEC.
    ///
    /// By default, the [global `log_schema.host_key` option][global_host_key] is used.
    ///
    /// [global_host_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.host_key
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "host_key")]
    pub host_key: String,

    /// Fields to be [added to Splunk index][splunk_field_index_docs].
    ///
    /// [splunk_field_index_docs]: https://docs.splunk.com/Documentation/Splunk/8.0.0/Data/IFXandHEC
    #[configurable(metadata(docs::advanced))]
    #[serde(default)]
    #[configurable(metadata(docs::examples = "field1", docs::examples = "field2"))]
    pub indexed_fields: Vec<String>,

    /// The name of the index to send events to.
    ///
    /// If not specified, the default index defined within Splunk is used.
    #[configurable(metadata(docs::examples = "{{ host }}", docs::examples = "custom_index"))]
    pub index: Option<Template>,

    /// The sourcetype of events sent to this sink.
    ///
    /// If unset, Splunk defaults to `httpevent`.
    #[configurable(metadata(docs::advanced))]
    #[configurable(metadata(docs::examples = "{{ sourcetype }}", docs::examples = "_json",))]
    pub sourcetype: Option<Template>,

    /// The source of events sent to this sink.
    ///
    /// This is typically the filename the logs originated from.
    ///
    /// If unset, the Splunk collector sets it.
    #[configurable(metadata(docs::advanced))]
    #[configurable(metadata(
        docs::examples = "{{ file }}",
        docs::examples = "/var/log/syslog",
        docs::examples = "UDP:514"
    ))]
    pub source: Option<Template>,

    #[configurable(derived)]
    pub encoding: EncodingConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub compression: Compression,

    #[configurable(derived)]
    #[serde(default)]
    pub batch: BatchConfig<SplunkHecDefaultBatchSettings>,

    #[configurable(derived)]
    #[serde(default)]
    pub request: TowerRequestConfig,

    #[configurable(derived)]
    pub tls: Option<TlsConfig>,

    #[configurable(derived)]
    #[serde(default)]
    pub acknowledgements: HecClientAcknowledgementsConfig,

    // This settings is relevant only for the `humio_logs` sink and should be left as `None`
    // everywhere else.
    #[serde(skip)]
    pub timestamp_nanos_key: Option<String>,

    /// Overrides the name of the log field used to retrieve the timestamp to send to Splunk HEC.
    /// When set to `“”`, a timestamp is not set in the events sent to Splunk HEC.
    ///
    /// By default, the [global `log_schema.timestamp_key` option][global_timestamp_key] is used.
    ///
    /// [global_timestamp_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.timestamp_key
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::sinks::splunk_hec::common::config_timestamp_key")]
    #[configurable(metadata(docs::examples = "timestamp", docs::examples = ""))]
    pub timestamp_key: OptionalValuePath,

    /// Passes the `auto_extract_timestamp` option to Splunk.
    ///
    /// This option is only relevant to Splunk v8.x and above, and is only applied when
    /// `endpoint_target` is set to `event`.
    ///
    /// Setting this to `true` causes Splunk to extract the timestamp from the message text
    /// rather than use the timestamp embedded in the event. The timestamp must be in the format
    /// `yyyy-mm-dd hh:mm:ss`.
    #[serde(default)]
    pub auto_extract_timestamp: Option<bool>,

    #[configurable(derived)]
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "default_endpoint_target")]
    pub endpoint_target: EndpointTarget,
}

const fn default_endpoint_target() -> EndpointTarget {
    EndpointTarget::Event
}

impl GenerateConfig for HecLogsSinkConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            default_token: "${VECTOR_SPLUNK_HEC_TOKEN}".to_owned().into(),
            endpoint: "endpoint".to_owned(),
            host_key: host_key(),
            indexed_fields: vec![],
            index: None,
            sourcetype: None,
            source: None,
            encoding: TextSerializerConfig::default().into(),
            compression: Compression::default(),
            batch: BatchConfig::default(),
            request: TowerRequestConfig::default(),
            tls: None,
            acknowledgements: Default::default(),
            timestamp_nanos_key: None,
            timestamp_key: config_timestamp_key(),
            auto_extract_timestamp: None,
            endpoint_target: EndpointTarget::Event,
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
impl SinkConfig for HecLogsSinkConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        if self.auto_extract_timestamp.is_some() && self.endpoint_target == EndpointTarget::Raw {
            return Err("`auto_extract_timestamp` cannot be set for the `raw` endpoint.".into());
        }

        let client = create_client(&self.tls, cx.proxy())?;
        let healthcheck = build_healthcheck(
            self.endpoint.clone(),
            self.default_token.inner().to_owned(),
            client.clone(),
        )
        .boxed();
        let sink = self.build_processor(client, cx)?;

        Ok((sink, healthcheck))
    }

    fn input(&self) -> Input {
        Input::new(self.encoding.config().input_type() & DataType::Log)
    }

    fn acknowledgements(&self) -> &AcknowledgementsConfig {
        &self.acknowledgements.inner
    }
}

impl HecLogsSinkConfig {
    pub fn build_processor(
        &self,
        client: HttpClient,
        cx: SinkContext,
    ) -> crate::Result<VectorSink> {
        let ack_client = if self.acknowledgements.indexer_acknowledgements_enabled {
            Some(client.clone())
        } else {
            None
        };

        let transformer = self.encoding.transformer();
        let serializer = self.encoding.build()?;
        let encoder = Encoder::<()>::new(serializer);
        let encoder = HecLogsEncoder {
            transformer,
            encoder,
            auto_extract_timestamp: self.auto_extract_timestamp.unwrap_or_default(),
        };
        let request_builder = HecLogsRequestBuilder {
            encoder,
            compression: self.compression,
        };

        let request_settings = self.request.unwrap_with(&TowerRequestConfig::default());
        let http_request_builder = Arc::new(HttpRequestBuilder::new(
            self.endpoint.clone(),
            self.endpoint_target,
            self.default_token.inner().to_owned(),
            self.compression,
        ));
        let http_service = ServiceBuilder::new()
            .settings(request_settings, HttpRetryLogic)
            .service(build_http_batch_service(
                client,
                Arc::clone(&http_request_builder),
                self.endpoint_target,
                self.auto_extract_timestamp.unwrap_or_default(),
            ));

        let service = HecService::new(
            http_service,
            ack_client,
            http_request_builder,
            self.acknowledgements.clone(),
        );

        let batch_settings = self.batch.into_batcher_settings()?;

        let sink = HecLogsSink {
            service,
            request_builder,
            context: cx,
            batch_settings,
            sourcetype: self.sourcetype.clone(),
            source: self.source.clone(),
            index: self.index.clone(),
            indexed_fields: self.indexed_fields.clone(),
            host: self.host_key.clone(),
            timestamp_nanos_key: self.timestamp_nanos_key.clone(),
            timestamp_key: self.timestamp_key.path.clone(),
            endpoint_target: self.endpoint_target,
        };

        Ok(VectorSink::from_event_streamsink(sink))
    }
}

#[cfg(test)]
mod tests {
    use super::HecLogsSinkConfig;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<HecLogsSinkConfig>();
    }
}
