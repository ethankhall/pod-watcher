use slog::{error, info, Logger};

use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{
    api::{Api, DeleteParams, Meta},
    Client,
};

type Result = std::result::Result<(), crate::errors::kubernetes::Error>;

enum KnownResource {
  Pod(Pod),
  Job(Job),
  ReplicaSet(ReplicaSet),
  Deployment(Deployment),
}

pub async fn delete_pod(
  logger: &Logger,
  namespace: &str,
  pod: Pod,
) -> Result {
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

async fn delete_resource<T>(logger: &Logger, target: T) -> Result
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