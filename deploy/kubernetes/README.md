# Kubernetes example

Three `rradius-server` nodes in one cluster, as a StatefulSet. Chapter 13 of the manual,
[docs/manual/13-kubernetes.md](../../docs/manual/13-kubernetes.md), explains how to apply it
and why it is built this way.

| File | What it is |
| --- | --- |
| `kustomization.yaml` | What `kubectl apply -k` applies, and the ConfigMap of `config/` |
| `statefulset.yaml` | The three pods `rradius-0` to `rradius-2`, their probes and volumes |
| `services.yaml` | `rradius-cluster` for the nodes, `rradius` for the devices |
| `pdb.yaml` | One pod at a time in voluntary disruptions |
| `config/` | One configuration per pod, the script and the migrations |
| `secret.example.yaml` | The Secret to create first |
| `hostnetwork-patch.yaml` | For clusters without a suitable load balancer (off) |
| `networkpolicy.yaml` | Keeps the metrics port to the nodes and Prometheus (off) |
