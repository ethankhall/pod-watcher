mod errors;

use clap::Clap;
use dotenv::dotenv;
use slog::{debug, error, info, o, trace, warn, Filter, Level, Logger};

use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{
    api::{Api, DeleteParams, Meta},
    Client,
};

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
    watcher_args: WatcherArgs,
    #[clap(flatten)]
    logging_opts: LoggingOpts,
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

    let result = match watch_k8s(log.new(o!()), opts).await {
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

async fn watch_k8s(logger: Logger, opts: Opts) -> Result<(), errors::kubernetes::Error> {
    let pod: Pod = get_pod_status(
        &opts.watcher_args.pod_namespace,
        &opts.watcher_args.pod_name,
    )
    .await?;
    let phase = pod
        .clone()
        .status
        .and_then(|status| status.phase)
        .unwrap_or_else(|| "None".to_string());

    debug!(
        logger,
        "Found pod {namespace}/{name}, current status {phase:?}",
        namespace = &opts.watcher_args.pod_namespace,
        name = &opts.watcher_args.pod_name,
        phase = &phase,
    );

    let pod_name = format!(
        "{}/{}",
        &opts.watcher_args.pod_namespace, &opts.watcher_args.pod_name
    );

    let new_logger = logger.new(o!("task" => "wait_for_crit", "pod" => pod_name.clone()));
    wait_for_critical_pod_to_exit(new_logger, &opts.watcher_args).await?;

    let new_logger = logger.new(o!("task" => "kill_pod", "pod" => pod_name));

    info!(new_logger, "Killing pod");
    delete_pod(&new_logger, &opts.watcher_args.pod_namespace, pod).await?;

    // If this code is running inside the pod it's trying to kill, will never get here
    info!(new_logger, "Pod Killed");
    Ok(())
}

enum KnownResource {
    Pod(Pod),
    Job(Job),
    ReplicaSet(ReplicaSet),
    Deployment(Deployment),
}

async fn delete_pod(
    logger: &Logger,
    namespace: &str,
    pod: Pod,
) -> Result<(), errors::kubernetes::Error> {
    let mut owner_refs = get_owners(&pod.meta());
    let mut delete_order: Vec<KnownResource> = Vec::new();
    delete_order.push(KnownResource::Pod(pod));

    while !owner_refs.is_empty() {
        let owner = owner_refs.pop().unwrap();

        let client = Client::try_default().await?;
        match owner.kind.as_str() {
            "ReplicaSet" => {
                let replica_set: Api<ReplicaSet> = Api::namespaced(client, &namespace);
                let target = replica_set.get(&owner.name).await?;
                for super_owner in get_owners(target.meta()) {
                    owner_refs.insert(0, super_owner);
                }
                delete_order.push(KnownResource::ReplicaSet(target));
            }
            "Job" => {
                let job: Api<Job> = Api::namespaced(client, &namespace);
                let target = job.get(&owner.name).await?;
                for super_owner in get_owners(target.meta()) {
                    owner_refs.insert(0, super_owner);
                }
                delete_order.push(KnownResource::Job(target));
            }
            "Deployment" => {
                let deployment: Api<Deployment> = Api::namespaced(client, &namespace);
                let target = deployment.get(&owner.name).await?;
                for super_owner in get_owners(target.meta()) {
                    owner_refs.insert(0, super_owner);
                }
                delete_order.push(KnownResource::Deployment(target));
            }
            _ => {
                error!(
                    logger,
                    "Unknown resource type: {}/{}. Unable to delete it!",
                    owner.api_version,
                    owner.kind
                );
                break;
            }
        }
    }

    delete_order.reverse();

    for target in delete_order {
        match target {
            KnownResource::Pod(target) => {
                delete_resource(logger, target).await?;
            }
            KnownResource::Job(target) => {
                delete_resource(logger, target).await?;
            }
            KnownResource::ReplicaSet(target) => {
                delete_resource(logger, target).await?;
            }
            KnownResource::Deployment(target) => {
                delete_resource(logger, target).await?;
            }
        }
    }
    Ok(())
}

async fn delete_resource<T>(logger: &Logger, target: T) -> Result<(), errors::kubernetes::Error>
where
    T: k8s_openapi::Resource + Clone + serde::de::DeserializeOwned + Meta,
{
    let client = Client::try_default().await?;
    let metadata = target.meta().clone();
    let namespace = &metadata.namespace.unwrap();
    let name = &metadata.name.unwrap();

    let resource_name = std::any::type_name::<T>().to_string();
    let last_path = resource_name.rfind(":").map(|x| x + 1).unwrap_or(0);
    let resource_name = resource_name[(last_path)..].to_string();

    info!(logger, "Deleting {} {}/{}", resource_name, namespace, &name);
    let api: Api<T> = Api::namespaced(client, namespace);
    api.delete(&name, &DeleteParams::default()).await?;
    Ok(())
}

fn get_owners(metadata: &ObjectMeta) -> Vec<OwnerReference> {
    match &metadata.owner_references {
        None => Vec::new(),
        Some(refs) => {
            let mut owner_refs = Vec::new();
            for a_ref in refs {
                if a_ref.controller.unwrap_or_default() {
                    owner_refs.push(a_ref.clone());
                }
            }
            owner_refs
        }
    }
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
