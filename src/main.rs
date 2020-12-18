mod delete;
mod errors;
mod istio;

use async_trait::async_trait;
use clap::Clap;
use dotenv::dotenv;
use slog::{debug, error, info, o, trace, warn, Filter, Level, Logger};

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
    #[clap(subcommand)]
    subcmd: SubCommand,
    #[clap(flatten)]
    logging_opts: LoggingOpts,
}

#[derive(Clap, Debug)]
enum SubCommand {
    /// Delete the pod when the condition is met
    DeletePod(DeletePod),

    /// Send the exit command to Istio Proxy having it exit
    StopIstio(StopIstio),
}

impl SubCommand {
    fn get_watcher_args<'a>(&'a self) -> &WatcherArgs {
        match self {
            SubCommand::DeletePod(d) => &d.watcher_args,
            SubCommand::StopIstio(s) => &s.watcher_args,
        }
    }
}

#[async_trait]
trait OnExit {
    async fn handle_exit(&self, logger: &Logger, pod: Pod) -> Result<(), crate::errors::Error>;
}

#[derive(Clap, Debug)]
pub struct DeletePod {
    #[clap(flatten)]
    watcher_args: WatcherArgs,
}

#[async_trait]
impl OnExit for DeletePod {
    async fn handle_exit(&self, logger: &Logger, pod: Pod) -> Result<(), crate::errors::Error> {
        Ok(delete::delete_pod(&logger, &self.watcher_args.pod_namespace, pod).await?)
    }
}

#[derive(Clap, Debug)]
pub struct StopIstio {
    #[clap(flatten)]
    watcher_args: WatcherArgs,
}

#[async_trait]
impl OnExit for StopIstio {
    async fn handle_exit(&self, logger: &Logger, _pod: Pod) -> Result<(), crate::errors::Error> {
        Ok(istio::stop_istio(&logger).await?)
    }
}

#[derive(Clap, Debug, PartialEq)]
enum KillCondition {
    Any,
    All,
}

#[derive(Clap, Debug)]
pub struct WatcherArgs {
    /// Name of the pod to watch
    #[clap(long, env = "POD_NAME")]
    pub pod_name: String,

    /// Namespace of the pod to watch
    #[clap(long, env = "POD_NAMESPACE")]
    pub pod_namespace: String,

    /// When should the pod be killed? If set to any, if any of the critical containers
    /// are terminated, the pod will be killed. When set to `all` all the containers
    /// must be terminated at the same time.
    #[clap(name = "when", long, arg_enum, default_value = "any")]
    kill_condition: KillCondition,

    pub critical_containers: Vec<String>,
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

    debug!(log, "Starting up....");

    // slog_stdlog uses the logger from slog_scope, so set a logger there
    let guard = slog_scope::set_global_logger(shared_logger);
    guard.cancel_reset();

    // register slog_stdlog as the log handler with the log crate
    slog_stdlog::init().unwrap();

    let result = match watch_k8s(log.new(o!()), opts.subcmd).await {
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

async fn watch_k8s(logger: Logger, opts: SubCommand) -> Result<(), errors::kubernetes::Error> {
    let watcher_args = opts.get_watcher_args();

    let pod: Pod = get_pod_status(&watcher_args.pod_namespace, &watcher_args.pod_name).await?;
    let phase = pod
        .clone()
        .status
        .and_then(|status| status.phase)
        .unwrap_or_else(|| "None".to_string());

    debug!(
        logger,
        "Found pod {namespace}/{name}, current status {phase:?}",
        namespace = &watcher_args.pod_namespace,
        name = &watcher_args.pod_name,
        phase = &phase,
    );

    let pod_name = format!("{}/{}", &watcher_args.pod_namespace, &watcher_args.pod_name);

    let new_logger = logger.new(o!("task" => "wait_for_crit", "pod" => pod_name.clone()));
    wait_for_critical_pod_to_exit(new_logger, &watcher_args).await?;

    let new_logger = logger.new(o!("task" => "kill_pod", "pod" => pod_name));

    info!(new_logger, "Stopping pod");
    delete::delete_pod(&new_logger, &watcher_args.pod_namespace, pod).await?;

    // If this code is running inside the pod it's trying to kill, will never get here
    info!(new_logger, "Dependencies stopped");
    Ok(())
}

async fn wait_for_critical_pod_to_exit(
    logger: Logger,
    args: &WatcherArgs,
) -> Result<(), errors::kubernetes::Error> {
    use tokio::time::{delay_for, Duration};

    loop {
        let should_kill_pod = match get_pod_status(&args.pod_namespace, &args.pod_name).await {
            Ok(pod) => are_critical_pods_dead(&logger, &pod, &args),
            Err(e) => {
                warn!(logger, "Unable to get details about pod. Ignoring error.";
                    "error" => e.to_string()
                );
                false
            }
        };

        if should_kill_pod {
            info!(logger, "Critical container(s) is/are died! Killing pod!",);
            break;
        }

        trace!(logger, "waiting for next tick");
        delay_for(Duration::from_millis(100)).await;
    }

    Ok(())
}

fn are_critical_pods_dead(logger: &Logger, pod: &Pod, args: &WatcherArgs) -> bool {
    use std::collections::BTreeSet;
    trace!(logger, "Pods current status: {:?}", &pod.status);

    let container_statuses = match &pod.status {
        None => {
            warn!(
                logger,
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
                logger,
                "Pod didn't return a container_statuses, assuming everything is running"
            );
            return false;
        }
    };

    let mut critical_containers: BTreeSet<String> = BTreeSet::new();
    for container in &args.critical_containers {
        critical_containers.insert(container.clone());
    }

    let mut critical_containers_dead = 0;
    for status in container_statuses {
        trace!(logger, "Critical containers: {:?}", &critical_containers);
        trace!(logger, "Processing container {}", &status.name);
        if critical_containers.contains(&status.name) {
            match &status.state {
                Some(state) => {
                    if let Some(terminated) = &state.terminated {
                        info!(logger, "Critical container {name} has exited!",
                         name = &status.name;
                         "terminated" => to_json(&terminated)
                        );
                        critical_containers_dead += 1;
                    }
                }
                None => {
                    warn!(
                        logger,
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
            logger,
            "Unable to find critical container(s): {}",
            critical_containers
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    match args.kill_condition {
        KillCondition::Any => critical_containers_dead != 0,
        KillCondition::All => critical_containers_dead == args.critical_containers.len(),
    }
}

async fn get_pod_status(namespace: &str, pod_name: &str) -> Result<Pod, errors::kubernetes::Error> {
    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    let pod: Pod = pods.get(pod_name).await?;
    Ok(pod)
}

fn to_json<T>(input: &T) -> String
where
    T: ?Sized + serde::Serialize,
{
    serde_json::to_string(&input).unwrap_or_default()
}
