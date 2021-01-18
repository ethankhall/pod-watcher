use slog::{info, Logger};

type Result = std::result::Result<(), crate::errors::reqwest::Error>;

pub async fn stop_istio(ip_address: String, logger: &Logger) -> Result {
    let client = reqwest::Client::new();
    let url = format!("http://{}:15000/quitquitquit", ip_address);
    let response = client.post(&url).send().await?;
    info!(logger, "Sent request to Istio to shut down.");
    info!(logger, "Response {:?}", response);
    Ok(())
}
