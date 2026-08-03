# debug-operator

`debug-operator` is a Rust/kube-rs admission operator for security research workflows where you need to inject debugging configuration into arbitrary Kubernetes workloads without editing the upstream Helm chart.

The shareable unit is a namespaced `DebugProfile` CRD. Profiles select Pods by namespace opt-in labels plus Pod labels, optional Helm release names, and optional container image globs. Matching Pods can receive environment variables, init containers, volumes, volume mounts, and explicit-proxy MITM sidecars.

## Current behavior

- Mutates only Pod `CREATE` admission requests.
- Requires the namespace label `debug-operator.hadron.re/enabled=true`.
- Skips the operator's own Pods and Pods already annotated as mutated.
- Uses deterministic JSON patches and fails admission on name conflicts by default.
- Runs optional Python bootstrap code as a per-profile, per-generation Kubernetes Job.
- Skips non-optional bootstrap profiles until declared output ConfigMaps/Secrets exist.
- Rechecks bootstrap outputs every 15 seconds.
- Supports `spec.network.mode: explicitProxy` by injecting a mitmproxy sidecar, proxy env vars, optional rules ConfigMap, and optional CA Secret trust hints.
- Declares `transparentProxy` and `dnsProxy` as CRD modes, but currently rejects them during admission until their privileged/DNS mutation logic is implemented.

## Development

```sh
cargo test
cargo run --bin crdgen
```

Build an image with:

```sh
make docker-build IMAGE=example.com/debug-operator:dev
```

## Install sketch

Generate and apply the CRD first:

```sh
cargo run --quiet --bin crdgen | kubectl apply -f -
```

Then install the operator manifests:

```sh
kubectl apply -k deploy
```

The provided manifests assume cert-manager is installed. Without cert-manager, create a `debug-operator-tls` secret yourself and set `webhooks[].clientConfig.caBundle` in `deploy/webhook.yaml`.

Or install with Helm:

```sh
helm upgrade --install debug-operator charts/debug-operator \
  --namespace debug-operator-system \
  --create-namespace
```

The chart installs the `DebugProfile` CRD from `charts/debug-operator/crds/` and templates RBAC, the operator Deployment, Service, webhook configuration, and optional cert-manager resources.

Opt a namespace into mutation:

```sh
kubectl label namespace default debug-operator.hadron.re/enabled=true
kubectl apply -f samples/env-and-init-profile.yaml
```

For the bootstrap-output sample, also apply `samples/bootstrap-rbac.yaml` so the bootstrap Job can write its ConfigMap.

## Traffic interception

The first implemented network mode is explicit proxy interception:

```yaml
spec:
  network:
    mode: explicitProxy
    proxy:
      image: mitmproxy/mitmproxy:latest
      rulesConfigMap: debug-proxy-rules
    tls:
      trustCa: true
      caSecret: debug-mitm-ca
```

This injects `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, optional `NO_PROXY`, CA trust env hints, a rules volume at `/debug-proxy/rules`, and a mitmproxy sidecar. The rules ConfigMap is expected to contain `addon.py`; bootstrap Jobs can generate that ConfigMap and the CA Secret.

The intended implementation order remains:

1. Explicit proxy sidecar.
2. Transparent proxy with an optional `NET_ADMIN` iptables init container.
3. Pod-scoped DNS proxying.
4. Advanced CoreDNS rewrite support only for admin-controlled clusters.
