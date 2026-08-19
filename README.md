# debug-operator

`debug-operator` is a Rust/kube-rs admission operator for security research workflows where you need to inject debugging configuration into arbitrary Kubernetes workloads without editing the upstream Helm chart.

The shareable unit is a namespaced `DebugProfile` CRD. Profiles select Pods by namespace opt-in labels plus Pod labels, optional Helm release names, and optional container image globs. Matching Pods can receive environment variables, init containers, volumes, volume mounts, and explicit-proxy MITM sidecars.

## Current behavior

- Mutates only Pod `CREATE` admission requests.
- Requires the namespace label `debug-operator.hadron.re/enabled=true`.
- Skips the operator's own Pods and Pods already annotated as mutated.
- Uses deterministic JSON patches and fails admission on name conflicts by default.
- Runs optional Python bootstrap code as a per-profile, per-generation Kubernetes Job.
- Installs declared `pythonDependencies` before executing bootstrap code.
- Publishes bootstrap files as operator-owned ConfigMaps after the Job succeeds, without granting the Job Kubernetes API permissions.
- Skips non-optional bootstrap profiles until their bootstrap Job succeeds and its outputs are published.
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

## Releases

Releases are automated from Conventional Commit messages on `main` using semantic-release. A release updates `Cargo.toml`, `Cargo.lock`, `charts/debug-operator/Chart.yaml`, and `CHANGELOG.md`, creates a GitHub Release/tag, then publishes the operator image to GHCR.

Published image tags:

- `ghcr.io/cfi2017/debug-operator:<version>`
- `ghcr.io/cfi2017/debug-operator:<major>`
- `ghcr.io/cfi2017/debug-operator:<major>.<minor>`
- `ghcr.io/cfi2017/debug-operator:latest`

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

Bootstrap code writes ConfigMap entries below
`$BOOTSTRAP_OUTPUT_DIRECTORY/configmaps/<config-map-name>/<key>`. The default output
directory is `/debug-operator-output`. After the Job succeeds, the operator applies
those files as ConfigMaps. For example:

```yaml
bootstrap:
  image: python:3.13-alpine
  pythonDependencies:
    - requests==2.32.5
  source: |
    import os
    path = os.path.join(
        os.environ["BOOTSTRAP_OUTPUT_DIRECTORY"],
        "configmaps", "my-settings"
    )
    os.makedirs(path, exist_ok=True)
    with open(os.path.join(path, "settings.json"), "w") as output:
        output.write('{"enabled":true}')
  outputConfigMaps:
    - my-settings
```

`outputConfigMaps` acts as an allowlist when set. UTF-8 files become ConfigMap `data`;
other files become `binaryData`. Bootstrap Jobs do not require ConfigMap RBAC. The
operator publishes each output into the DebugProfile namespace and every namespace
matching `selector.namespaceOptIn`, so cross-namespace target workloads receive the
same ConfigMap content.

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

This injects `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, optional `NO_PROXY`, CA trust env hints, a rules volume at `/debug-proxy/rules`, and a mitmproxy sidecar. The rules ConfigMap is expected to contain `addon.py`; bootstrap Jobs can generate that ConfigMap.

The intended implementation order remains:

1. Explicit proxy sidecar.
2. Transparent proxy with an optional `NET_ADMIN` iptables init container.
3. Pod-scoped DNS proxying.
4. Advanced CoreDNS rewrite support only for admin-controlled clusters.
