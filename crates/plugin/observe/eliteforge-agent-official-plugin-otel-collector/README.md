# OTLP Collector plugin

This plugin receives HTTP/protobuf exports at `/v1/traces` and `/v1/metrics` and
writes them through Core's `TelemetryStoreService`. It has no database driver,
application identity model or fixed retention policy. Install a storage plugin
first and set its ID in `CollectorPlugin::storage_plugin_id`.

The host provides the bind address, authorization header, request concurrency and
cleanup interval in `CollectorConfig`. `CollectorService` exposes the bound
endpoint and authorization to the managed OTel runtime or host integration.
Requests support gzip, with 8 MiB limits before and after decompression and a
50,000-record per-request limit. Stored metric count is controlled solely by the
storage implementation's retention policy, not this request limit.

An export is acknowledged only after its storage transaction succeeds. Storage
failure returns HTTP 503, concurrency exhaustion 429, unauthorized exports 401,
and invalid protobuf/IDs/timestamps 400. Trace/span retries overwrite the same
identity; metric point IDs are derived deterministically from their content.
Raw OTLP span/resource/scope data and complete metric point structures are retained.
Int64 metric values use decimal strings at the JSON boundary.

The plugin owns its receiver and cleanup task. Shutdown cancels and awaits both
before its storage dependency closes. The managed runtime depends on this plugin
when resolving its endpoint locally, so SDK shutdown exports can still be received.
OTLP/gRPC, OTLP/JSON and logs are not implemented.
