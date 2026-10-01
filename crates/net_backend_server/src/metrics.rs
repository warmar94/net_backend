//! Prometheus metrics at `/metrics` (`metrics.enabled`, off by default).
//!
//! The server records through the [`metrics`] facade (`nbs_http_requests_total`,
//! `nbs_http_request_duration_seconds`, labelled by method, route pattern and status). With
//! metrics enabled it installs a Prometheus recorder as the process-wide recorder, once; if the
//! app already installed its own recorder, the framework's metrics go there and `/metrics` is not
//! served (logged). Games record their own metrics with the same facade.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<Option<PrometheusHandle>> = OnceLock::new();

/// The process-wide Prometheus handle, installing the recorder on first use.
pub(crate) fn handle() -> Option<PrometheusHandle> {
    HANDLE
        .get_or_init(|| {
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            match metrics::set_global_recorder(recorder) {
                Ok(()) => Some(handle),
                Err(_) => {
                    tracing::warn!("another metrics recorder is installed; /metrics is not served");
                    None
                }
            }
        })
        .clone()
}
