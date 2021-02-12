mod cleanup;
mod errors;

use chrono::prelude::*;
use clap::{ArgGroup, Clap};
use dotenv::dotenv;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::delay_for;

use k8s_openapi::api::core::v1::Pod;
use kube::{api::Api, Client};

use tracing::{instrument, Level, debug, error, info, trace, warn};


#[derive(Clap, Debug)]
#[clap(group = ArgGroup::new("logging"))]
pub struct LoggingOpts {
    /// A level of verbosity, and can be used multiple times
    #[clap(short, long, parse(from_occurrences), global(true), group = "logging")]
    pub verbose: u64,

    /// Enable warn logging
    #[clap(short, long, global(true), group = "logging")]
    pub warn: bool,

    /// Disable everything but error logging
    #[clap(short, long, global(true), group = "logging")]
    pub error: bool,
}

impl LoggingOpts {
    fn to_level_filter(&self) -> Level {
        if self.error {
            Level::ERROR
        } else if self.warn {
            Level::WARN
        } else if self.verbose == 0 {
            Level::INFO
        } else if self.verbose == 1 {
            Level::DEBUG
        } else {
            Level::TRACE
        }
    }
}

#[derive(Clap, Debug)]
#[clap(author, about, version)]
struct Opts {
    #[clap(flatten)]
    args: WatcherArgs,
    #[clap(flatten)]
    logging_opts: LoggingOpts,
}

#[derive(Debug, Clone, PartialEq)]
enum KillCondition {
    Any,
    All,
}

#[derive(Clap, Debug)]
pub struct WatcherArgs {
    /// Name of the Istio Container inside the Pod.
    /// When the container is found, it will be shut down nicely.
    #[clap(long, default_value = "istio-proxy", env = "ISTIO_CONTAINER_NAME")]
    istio_container_name: String,

    #[clap(long, default_value = "5000", env = "ISTIO_DEADLINE")]
    istio_deadline_ms: u32,

    #[clap(long, default_value = "1000", env = "CONTAINER_DEADLINE")]
    critical_deadline: i64,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let exit_code = run().await;
    std::process::exit(exit_code);
}

async fn run() -> i32 {
    let opts = Opts::parse();
    let level = opts.logging_opts.to_level_filter();

    tracing_subscriber::fmt()
        .json()
        .with_max_level(level)
        .with_current_span(false)
        .init();

    info!("Starting up....");

    let result = match watch_k8s(&opts.args).await {
        Ok(_) => 0,
        Err(e) => {
            error!("Unrecoverable error! Error: {:?}", e.to_string());
            1
        }
    };

    info!("Exiting code {}", result);

    result
}

#[instrument(skip(opts))]
async fn watch_k8s(opts: &WatcherArgs) -> Result<(), errors::kubernetes::Error> {
    let cleanup = cleanup::CleanupPod::new(&opts.istio_container_name, opts.istio_deadline_ms);
    let mut pods_being_deleted: BTreeMap<String, i64> = BTreeMap::new();

    loop {
        check_all_pods(opts.critical_deadline, &mut pods_being_deleted, &cleanup).await?;

        delay_for(Duration::from_secs(5)).await;

        let now = Utc::now().timestamp_millis();

        let mut keys_to_remove = Vec::new();

        for key in pods_being_deleted.keys() {
            if pods_being_deleted.get(key).unwrap_or(&0) < &now {
                keys_to_remove.push(key.clone());
            }
        }

        for key in keys_to_remove {
            pods_being_deleted.remove(&key);
        }
    }
}

#[instrument(skip(cleanup))]
async fn check_all_pods(
    critical_deadline: i64,
    pods_being_deleted: &mut BTreeMap<String, i64>,
    cleanup: &cleanup::CleanupPod,
) -> Result<(), errors::kubernetes::Error> {
    info!("Fetching pod statuses");
    let watched_pods = get_pods_status().await?;
    for pod_container in watched_pods {
        let uid = match &pod_container.pod.metadata.uid {
            Some(uuid) => uuid.to_string(),
            None => continue,
        };

        if pods_being_deleted.contains_key(&uid) {
            debug!("Ignoring pod as it's being deleted.");
            continue;
        }

        {
            let pod_container = pod_container.clone();
            let critical_containers = &pod_container.critical_containers;
            let critical_containers = critical_containers.join(",");
            info!(
                "Processing pod {critical_containers:?}",
                critical_containers = &critical_containers
            );
        };

        if pod_container.should_be_terminated(&critical_deadline) {
            if let Err(e) = cleanup.cleanup_pod(&pod_container.pod).await {
                error!(
                    "There was an error while trying to delete Pos. Error: {:?}",
                    e
                );
            }

            pods_being_deleted
                .entry(uid)
                .or_insert(Utc::now().timestamp_millis() + 10_000);
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct PodContainer {
    pod: Pod,
    name: String,
    namespace: String,
    condition: KillCondition,
    critical_containers: Vec<String>,
}

impl PodContainer {
    #[instrument]
    fn should_be_terminated(&self, crit_deadline: &i64) -> bool {
        trace!("Pods current status: {:?}", &self.pod.status);

        let container_statuses = match &self.pod.status {
            None => {
                warn!("Pod didn't return a status, assuming everything is running");
                return false;
            }
            Some(status) => &status.container_statuses,
        };

        let container_statuses = match container_statuses {
            Some(container_statuses) => container_statuses,
            None => {
                warn!("Pod didn't return a container_statuses, assuming everything is running");
                return false;
            }
        };

        let mut critical_containers: BTreeSet<String> = BTreeSet::new();
        for container in &self.critical_containers {
            critical_containers.insert(container.clone());
        }

        let mut critical_containers_dead = 0;
        for status in container_statuses {
            trace!("Critical containers: {:?}", &critical_containers);
            trace!("Processing container {}", &status.name);
            if critical_containers.contains(&status.name) {
                match &status.state {
                    Some(state) => {
                        if let Some(terminated) = &state.terminated {
                            let past_deadline = match &terminated.finished_at {
                                Some(time) => {
                                    time.0.timestamp_millis() + crit_deadline
                                        > Utc::now().timestamp_millis()
                                }
                                None => true,
                            };

                            if past_deadline {
                                critical_containers_dead += 1;
                                info!(
                                    "Critical container {name} has exited, and deadline passed!",
                                    name = &status.name
                                );
                            } else {
                                info!( "Critical container {name} has exited, but hasn't passed the deadline.", name = &status.name);
                            }
                        }
                    }
                    None => {
                        warn!(
                            "Critical container {name} didn't return a status, assuming it's ok",
                            name = &status.name
                        );
                    }
                }
                critical_containers.remove(&status.name);
            }
        }

        if !critical_containers.is_empty() {
            warn!(
                "Unable to find critical container(s): {}",
                &critical_containers
                    .iter()
                    .map(|x| x.clone())
                    .collect::<Vec<String>>()
                    .join(", ")
            );
        }

        match self.condition {
            KillCondition::Any => critical_containers_dead != 0,
            KillCondition::All => critical_containers_dead == self.critical_containers.len(),
        }
    }
}

#[instrument]
async fn get_pods_status() -> Result<Vec<PodContainer>, errors::kubernetes::Error> {
    use kube::api::ListParams;

    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::all(client);
    let pods = pods.list(&ListParams::default()).await?;

    let watched_pods: Vec<PodContainer> = pods
        .items
        .into_iter()
        .filter(|pod| {
            if pod.metadata.name.is_none() || pod.metadata.namespace.is_none() {
                return false;
            }
            match &pod.metadata.annotations {
                Some(annotation) => annotation.contains_key("podwatcher/critical-containers"),
                None => false,
            }
        })
        .map(|pod| {
            let name = pod.metadata.name.clone().unwrap();
            let namespace = pod.metadata.namespace.clone().unwrap();
            let annotations = pod.metadata.annotations.clone().unwrap();
            let critial_annotation: &String =
                annotations.get("podwatcher/critical-containers").unwrap();
            let critical_containers = critial_annotation
                .replace(" ", "")
                .split('.')
                .map(|x| x.to_string())
                .collect();

            let default_any = "any".to_string();
            let condition = annotations
                .get("podwatcher/condition")
                .unwrap_or(&default_any);

            let condition = match condition.to_lowercase().as_str() {
                "any" | "" => KillCondition::Any,
                "all" => KillCondition::All,
                e => {
                    warn!("Unable to parse {}, assuming any", e);
                    KillCondition::Any
                }
            };

            PodContainer {
                pod,
                name,
                namespace,
                condition,
                critical_containers,
            }
        })
        .collect();

    Ok(watched_pods)
}