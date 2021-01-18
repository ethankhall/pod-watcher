mod delete;
mod istio;

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::core::v1::PodStatus;
use slog::{warn, Logger};
use std::time::Duration;

use tokio::time::delay_for;

pub struct CleanupPod {
    istio_container_name: String,
    istio_deadline_ms: u32,
}

impl CleanupPod {
    pub fn new(istio_container_name: &str, istio_deadline_ms: u32) -> Self {
        Self {
            istio_container_name: istio_container_name.to_string(),
            istio_deadline_ms,
        }
    }

    pub async fn cleanup_pod(
        &self,
        logger: &Logger,
        pod: &Pod,
    ) -> Result<(), crate::errors::Error> {
        if let Some(ip) = self.get_istio_container_ip(&pod, &logger) {
            istio::stop_istio(ip, &logger).await?;
            delay_for(Duration::from_millis(self.istio_deadline_ms.into())).await;
        }

        delete::delete_pod(logger, pod).await?;

        Ok(())
    }

    fn get_istio_container_ip(&self, pod: &Pod, logger: &Logger) -> Option<String> {
        let status: &PodStatus = match &pod.status {
            None => {
                warn!(logger, "Pod didn't return a status, will not disable Istio");
                return None;
            }
            Some(status) => status,
        };

        match &status.container_statuses {
            None => None,
            Some(statuses) => {
                if statuses
                    .iter()
                    .any(|status| status.name == self.istio_container_name)
                {
                    status.pod_ip.clone()
                } else {
                    None
                }
            }
        }
    }
}
