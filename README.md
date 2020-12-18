# Pod Watcher

This tool is intended to make it easier to have "sidecars" shut down
when the "critical container" have exited.

This tool has two ways to exit, delete the pod or tell Istio to shutdown.

## Delete Pod

This is usually used when there is a Job with many different containers running,
and those containers don't have a shutdown command.

### Usage

```yaml
spec:
  containers:
    ...
    - name: pod-watcher
      image: docker.pkg.github.com/ethankhall/pod-watcher/pod-watcher:latest
      command: [ "/app/pod-watcher", "delete-pod", "job-container-name"]
      resources:
        request:
          cpu: 10m
          memory: 32Mi
        limit:
          cpu: 10m
          memory: 128Mi
      env:
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: POD_NAMESPACE
          valueFrom:
            fieldRef:
              fieldPath: metadata.namespace
```

## Stop Istio

If you have a Job that has a primary container, and then an Istio container.
When in this mode the Istio container will sent an HTTP command that will cause
the pod to shutdown allowing the Job to finish successfully.

### Usage

```yaml
  containers:
    ...
    - name: pod-watcher
      image: docker.pkg.github.com/ethankhall/pod-watcher/pod-watcher:latest
      command: [ "/app/pod-watcher", "stop-istio", "job-container-name"]
      resources:
        request:
          cpu: 10m
          memory: 32Mi
        limit:
          cpu: 10m
          memory: 128Mi
      env:
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: POD_NAMESPACE
          valueFrom:
            fieldRef:
              fieldPath: metadata.namespace
```