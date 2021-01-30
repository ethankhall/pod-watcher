# Pod Watcher

This tool is intended to help emulate the concept of "critical containers".

That means that when a critical container inside a pod stops, this code
will see that (after a timeout) and if the container doesn't restart, the
pod will be deleted.

This tool requires that the pod be annotated with
`podwatcher/critical-containers: "container1"` where the value is a comma
seperated list.

When there are multiple containers that are in the critical container list,
you can choose if `any` or `all` containers need to have terminated. You can
use `podwatcher/condition` to configure it. By default, `any` will be used.

## Deployment

This project uses Kustomize for deployment, it's recommended to update the
namespace. It will default to using `pod-watcher-system`, if you don't change
it you'll need to create the namespace.

A ClusterRole and ClusterRoleBinding will be created to allow the created
ServiceAccount to access Pod and Jobs.s