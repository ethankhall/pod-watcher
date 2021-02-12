use tracing::{info};

type Result = std::result::Result<(), crate::errors::reqwest::Error>;

pub async fn stop_istio(ip_address: String) -> Result {
    let client = reqwest::Client::new();
    let url = format!("http://{}:15000/quitquitquit", ip_address);
    let response = client.post(&url).send().await?;
    info!("Sent request to Istio to shut down.");
    info!("Response {:?}", response);
    Ok(())
}
