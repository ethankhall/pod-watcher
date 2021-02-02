mod cleanup;
mod errors;

use chrono::prelude::*;
use clap::Clap;
use dotenv::dotenv;
use slog::{debug, error, info, o, trace, warn, Filter, Level, Logger};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::delay_for;

use k8s_openapi::api::core::v1::Pod;
use kube::{api::Api, Client};

#[derive(Clap, Debug)]
pub struct LoggingOpts {
    /// A level of verbosity, and can be used multiple times
    #[clap(short, long, parse(from_occurrences), group = "logging")]
    verbose: u64,

    /// Enable all logging
    #[clap(short, long, group = "logging")]
    debug: bool,

    /// Disable everything but error logging
    #[clap(short, long, group = "logging")]
    error: bool,
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
    use slog::Drain;
    dotenv().ok();

    let opts = Opts::parse();
    let decorator = slog_term::TermDecorator::new().build();
    let drain = std::sync::Mutex::new(slog_term::FullFormat::new(decorator).build()).fuse();
    let drain = Filter(drain, |record| {
        record.module().starts_with(module_path!()) || record.level().is_at_least(Level::Info)
    })
    .fuse();
    let drain = Filter(drain, |record| record.level().is_at_least(Level::Debug)).fuse();
    let log = Logger::root(
        drain,
        o!("version" => env!("CARGO_PKG_VERSION"), "module" => slog::FnValue(module_and_line)),
    );
    let shared_logger = log.new(slog::o!());

    info!(log, "Starting up....");

    // slog_stdlog uses the logger from slog_scope, so set a logger there
    let guard = slog_scope::set_global_logger(shared_logger);
    guard.cancel_reset();

    // register slog_stdlog as the log handler with the log crate
    slog_stdlog::init().unwrap();

    let result = match watch_k8s(log.new(o!()), &opts.args).await {
        Ok(_) => 0,
        Err(e) => {
            error!(log, "Unrecoverable error!"; "error" => e.to_string());
            1
        }
    };

    info!(log, "Exiting"; "code" => result);

    std::process::exit(result);
}

pub fn module_and_line(record: &slog::Record) -> String {
    format!("{}:{}", record.module(), record.line())
}

async fn watch_k8s(logger: Logger, opts: &WatcherArgs) -> Result<(), errors::kubernetes::Error> {
    let cleanup = cleanup::CleanupPod::new(&opts.istio_container_name, opts.istio_deadline_ms);
    let mut pods_being_deleted: BTreeMap<String, i64> = BTreeMap::new();

    loop {
        info!(logger, "Fetching pod statuses");
        let watched_pods = get_pods_status(&logger).await?;
        for pod_container in watched_pods {
            let uid = match &pod_container.pod.metadata.uid {
                Some(uuid) => uuid.to_string(),
                None => continue,
            };

            if pods_being_deleted.contains_key(&uid) {
                debug!(pod_container.logger, "Ignoring pod as it's being deleted.");
                continue;
            }

            {
                let pod_container = pod_container.clone();
                let critical_containers = &pod_container.critical_containers;
                let critical_containers = critical_containers.join(",");
                info!(
                    pod_container.logger,
                    "Processing pod {critical_containers:?}",
                    critical_containers = &critical_containers
                );
            };

            if pod_container.should_be_terminated(&opts.critical_deadline) {
                if let Err(e) = cleanup
                    .cleanup_pod(&pod_container.logger, &pod_container.pod)
                    .await
                {
                    error!(pod_container.logger, "There was an error while trying to delete Pos."; "error" => %e);
                }

                pods_being_deleted
                    .entry(uid)
                    .or_insert(Utc::now().timestamp_millis() + 10_000);
            }
        }

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

#[derive(Debug, Clone)]
struct PodContainer {
    pod: Pod,
    name: String,
    namespace: String,
    logger: Logger,
    condition: KillCondition,
    critical_containers: Vec<String>,
}

impl PodContainer {
    fn should_be_terminated(&self, crit_deadline: &i64) -> bool {
        trace!(self.logger, "Pods current status: {:?}", &self.pod.status);

        let container_statuses = match &self.pod.status {
            None => {
                warn!(
                    self.logger,
                    "Pod didn't return a status, assuming everything is running"
                );
                return false;
            }
            Some(status) => &status.container_statuses,
        };

        let container_statuses = match container_statuses {
            Some(container_statuses) => container_statuses,
            None => {
                warn!(
                    self.logger,
                    "Pod didn't return a container_statuses, assuming everything is running"
                );
                return false;
            }
        };

        let mut critical_containers: BTreeSet<String> = BTreeSet::new();
        for container in &self.critical_containers {
            critical_containers.insert(container.clone());
        }

        let mut critical_containers_dead = 0;
        for status in container_statuses {
            trace!(
                self.logger,
                "Critical containers: {:?}",
                &critical_containers
            );
            trace!(self.logger, "Processing container {}", &status.name);
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
                                info!(self.logger, "Critical container {name} has exited, and deadline passed!",
                                 name = &status.name;
                                 "terminated" => to_json(&terminated)
                                );
                            } else {
                                info!(self.logger, "Critical container {name} has exited, but hasn't passed the deadline.", name = &status.name; "terminated" => to_json(&terminated));
                            }
                        }
                    }
                    None => {
                        warn!(
                            self.logger,
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
                self.logger,
                "Unable to find critical container(s): {}",
                critical_containers
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        match self.condition {
            KillCondition::Any => critical_containers_dead != 0,
            KillCondition::All => critical_containers_dead == self.critical_containers.len(),
        }
    }
}

async fn get_pods_status(logger: &Logger) -> Result<Vec<PodContainer>, errors::kubernetes::Error> {
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

            let logger = logger.new(o!("path" => format!("{}/{}", namespace, name)));

            let default_any = "any".to_string();
            let condition = annotations
                .get("podwatcher/condition")
                .unwrap_or(&default_any);

            let condition = match condition.to_lowercase().as_str() {
                "any" | "" => KillCondition::Any,
                "all" => KillCondition::All,
                e => {
                    warn!(logger, "Unable to parse {}, assuming any", e);
                    KillCondition::Any
                }
            };

            PodContainer {
                pod,
                name,
                namespace,
                logger,
                condition,
                critical_containers,
            }
        })
        .collect();

    Ok(watched_pods)
}

fn to_json<T>(input: &T) -> String
where
    T: ?Sized + serde::Serialize,
{
    serde_json::to_string(&input).unwrap_or_default()
}
