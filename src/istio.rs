use slog::{info, Logger};

type Result = std::result::Result<(), crate::errors::reqwest::Error>;

pub async fn stop_istio(logger: &Logger) -> Result {
  let client = reqwest::Client::new();
  let response = client.post("http://127.0.0.1:15000/quitquitquit").send().await?;
  info!(logger, "Sent request to Istio to shut down.");
  info!(logger, "Response {:?}", response);
  Ok(())
}