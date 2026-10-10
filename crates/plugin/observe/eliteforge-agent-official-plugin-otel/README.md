# OpenTelemetry plugin

`TelemetryPlugin::new` instruments Agent runs using host-supplied trace and meter
providers. It does not create, install globally or shut down those providers.
`TelemetryConfig` controls observation limits, identifier recording and structured
error report recording. Hosts storing minimal diagnostics can set
`record_error_reports: false` while retaining error kinds.

The optional `runtime` feature adds `OtelRuntimePlugin` and `OtelRuntimeService`.
The plugin creates SDK providers, a standard OTLP/HTTP protobuf SpanExporter and
MetricExporter, a batch span processor and a PeriodicReader. Defaults are a
15-second metric export interval and a 5-second export timeout.

Set `OtelRuntimeConfig::endpoint` and headers for an external collector, or leave
endpoint unset to resolve the local `otel.collector` service. Local composition
uses plugin dependencies: telemetry storage, Collector, then OTel runtime. The
host supplies resource attributes and controls proxy use. Each runtime has a
`service.instance.id` and the current `process.pid`.

With `process_metrics: true`, the runtime samples the current process, recording
`process.cpu.utilization` (fraction of one CPU core, possibly above 1) and
`process.memory.usage` (resident bytes) through its MeterProvider. The first sample
follows a one-second CPU warmup, with subsequent samples at the export interval.
Process metrics default to disabled. Shutdown stops sampling, then shuts down and
flushes both owned providers before the local Collector and storage close.
