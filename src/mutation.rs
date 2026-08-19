use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use json_patch::{AddOperation, Patch, PatchOperation, ReplaceOperation};
use jsonptr::PointerBuf;
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, KeyToPath,
    Namespace, Pod, SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use kube::{
    ResourceExt,
    api::Api,
    core::admission::{AdmissionRequest, AdmissionResponse, Operation},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::api::{
    BinaryPatchSpec, ConflictPolicy, DebugProfile, InjectionSpec, MANAGED_BY_LABEL,
    MUTATED_BY_ANNOTATION, MUTATION_HASH_ANNOTATION, NetworkMode, NetworkSpec, OPERATOR_NAME,
    PROFILE_ANNOTATION, PROFILE_GENERATION_ANNOTATION, ProfileStore,
};

const PATCH_TOOLS_VOLUME: &str = "debug-operator-patch-tools";
const PATCH_OUTPUT_VOLUME: &str = "debug-operator-patches";
const PATCH_INSTALLER: &str = "debug-operator-patcher-install";

pub async fn mutate_pod(
    req: &AdmissionRequest<Pod>,
    profiles: &ProfileStore,
    namespaces: &Api<Namespace>,
) -> anyhow::Result<AdmissionResponse> {
    if req.operation != Operation::Create {
        return Ok(AdmissionResponse::from(req));
    }

    let Some(pod) = req.object.as_ref() else {
        return Ok(AdmissionResponse::from(req));
    };

    if is_operator_pod(pod) || already_mutated(pod) {
        return Ok(AdmissionResponse::from(req));
    }

    let namespace = req
        .namespace
        .clone()
        .or_else(|| pod.namespace())
        .context("pod has no namespace")?;
    let ns = namespaces.get(&namespace).await?;
    let ns_labels = ns.metadata.labels.unwrap_or_default();

    for profile in &profiles.profiles {
        if profile_matches(profile, pod, &ns_labels) {
            let patch = build_patch(profile, pod)?;
            let mut response = AdmissionResponse::from(req);
            response
                .audit_annotations
                .insert("profile".to_string(), profile.name_any());
            if patch.0.is_empty() {
                return Ok(response);
            }
            return Ok(response.with_patch(patch)?);
        }
    }

    Ok(AdmissionResponse::from(req))
}

pub fn profile_matches(
    profile: &DebugProfile,
    pod: &Pod,
    namespace_labels: &BTreeMap<String, String>,
) -> bool {
    let spec = &profile.spec;
    if spec.bootstrap.is_some() && !spec.bootstrap.as_ref().is_some_and(|b| b.optional) {
        if !profile.status.as_ref().is_some_and(|status| status.ready) {
            return false;
        }
    }

    if !labels_match(&spec.selector.namespace_opt_in, namespace_labels) {
        return false;
    }

    let pod_labels = pod.metadata.labels.as_ref().cloned().unwrap_or_default();
    if !labels_match(&spec.selector.pod_labels, &pod_labels) {
        return false;
    }

    if !spec.selector.helm_releases.is_empty() {
        let release = pod_labels.get("app.kubernetes.io/instance").or_else(|| {
            pod.metadata
                .annotations
                .as_ref()?
                .get("meta.helm.sh/release-name")
        });
        if !release.is_some_and(|value| spec.selector.helm_releases.iter().any(|r| r == value)) {
            return false;
        }
    }

    if !spec.selector.image_globs.is_empty() {
        let images = pod
            .spec
            .as_ref()
            .map(|spec| {
                spec.containers
                    .iter()
                    .map(|c| c.image.as_deref().unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !images.iter().any(|image| {
            spec.selector
                .image_globs
                .iter()
                .any(|glob| glob_match(glob, image))
        }) {
            return false;
        }
    }

    true
}

pub fn build_patch(profile: &DebugProfile, pod: &Pod) -> anyhow::Result<Patch> {
    let injection = &profile.spec.injection;
    let mut ops = Vec::new();

    add_annotations(&mut ops, pod, profile, injection)?;
    add_network(&mut ops, pod, profile)?;
    add_env(&mut ops, pod, injection, &profile.spec.conflict_policy)?;
    add_init_containers(&mut ops, pod, injection, &profile.spec.conflict_policy)?;
    add_volumes(&mut ops, pod, injection, &profile.spec.conflict_policy)?;
    add_volume_mounts(&mut ops, pod, injection, &profile.spec.conflict_policy)?;
    add_binary_patches(&mut ops, pod, injection, &profile.spec.conflict_policy)?;

    Ok(Patch(ops))
}

fn add_binary_patches(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    injection: &InjectionSpec,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if injection.binary_patches.is_empty() {
        return Ok(());
    }
    let spec = pod.spec.as_ref().context("pod has no spec")?;
    let mut destinations = BTreeSet::new();
    for patch in &injection.binary_patches {
        validate_binary_patch(patch)?;
        if !destinations.insert((patch.container.as_str(), patch.path.as_str())) {
            bail!(
                "multiple binary patches target container '{}' path '{}'",
                patch.container,
                patch.path
            );
        }
    }

    for reserved in [PATCH_TOOLS_VOLUME, PATCH_OUTPUT_VOLUME] {
        if injection
            .volumes
            .iter()
            .any(|volume| volume.name == reserved)
        {
            bail!("volume name '{reserved}' is reserved for binary patches");
        }
    }
    if injection
        .volumes
        .iter()
        .any(|volume| volume.name.starts_with("debug-patch-input-"))
    {
        bail!("volume names beginning with 'debug-patch-input-' are reserved for binary patches");
    }
    if injection.init_containers.iter().any(|container| {
        container.name == PATCH_INSTALLER || container.name.starts_with("debug-binary-patch-")
    }) {
        bail!("debug operator binary patch initContainer names are reserved");
    }
    for patch in &injection.binary_patches {
        if injection
            .volume_mounts
            .iter()
            .any(|mount| mount.mount_path == patch.path)
        {
            bail!(
                "injection volumeMount already targets binary patch path '{}'",
                patch.path
            );
        }
    }

    let mut volumes = vec![
        Volume {
            name: PATCH_TOOLS_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: PATCH_OUTPUT_VOLUME.to_string(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
    ];
    for (index, patch) in injection.binary_patches.iter().enumerate() {
        if let Some(source) = patch.replace_from.as_ref() {
            volumes.push(Volume {
                name: format!("debug-patch-input-{index}"),
                config_map: Some(ConfigMapVolumeSource {
                    name: source.config_map_key_ref.name.clone(),
                    optional: source.config_map_key_ref.optional,
                    items: Some(vec![KeyToPath {
                        key: source.config_map_key_ref.key.clone(),
                        path: "value".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }
    add_volumes_from_slice(ops, pod, &volumes, policy)?;

    let patcher_image = injection
        .binary_patches
        .iter()
        .find_map(|patch| patch.patcher_image.clone())
        .or_else(|| std::env::var("DEBUG_OPERATOR_IMAGE").ok())
        .unwrap_or_else(|| {
            format!(
                "ghcr.io/cfi2017/debug-operator:{}",
                env!("CARGO_PKG_VERSION")
            )
        });
    if injection.binary_patches.iter().any(|patch| {
        patch
            .patcher_image
            .as_ref()
            .is_some_and(|image| image != &patcher_image)
    }) {
        bail!("all binary patches in a profile must use the same patcherImage");
    }

    info!(
        pod = %pod.name_any(),
        patch_count = injection.binary_patches.len(),
        image = %patcher_image,
        "injecting binary patch init chain"
    );

    let installer = Container {
        name: PATCH_INSTALLER.to_string(),
        image: Some(patcher_image),
        command: Some(vec!["/usr/local/bin/debug-operator".to_string()]),
        args: Some(vec![
            "install-patcher".to_string(),
            "/debug-tools/debug-patcher".to_string(),
            "/debug-patches".to_string(),
        ]),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            run_as_non_root: Some(false),
            run_as_user: Some(0),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        volume_mounts: Some(vec![
            VolumeMount {
                name: PATCH_TOOLS_VOLUME.to_string(),
                mount_path: "/debug-tools".to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: PATCH_OUTPUT_VOLUME.to_string(),
                mount_path: "/debug-patches".to_string(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    add_generated_init_container(ops, pod, &installer, policy)?;

    for (index, patch) in injection.binary_patches.iter().enumerate() {
        let target_index = spec
            .containers
            .iter()
            .position(|container| container.name == patch.container)
            .with_context(|| format!("binary patch container '{}' not found", patch.container))?;
        let target = &spec.containers[target_index];
        info!(
            pod = %pod.name_any(),
            container = %patch.container,
            path = %patch.path,
            expected_matches = patch.expected_matches,
            replacement_source = if patch.replace_from.is_some() { "configMap" } else { "inline" },
            "injecting binary patch"
        );
        let output_name = format!("patch-{index}");
        let replacement = patch
            .replace_hex
            .clone()
            .unwrap_or_else(|| format!("@/debug-patch-inputs/{index}/value"));
        let mut patcher_mounts = vec![
            VolumeMount {
                name: PATCH_TOOLS_VOLUME.to_string(),
                mount_path: "/debug-tools".to_string(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: PATCH_OUTPUT_VOLUME.to_string(),
                mount_path: "/debug-patches".to_string(),
                ..Default::default()
            },
        ];
        if patch.replace_from.is_some() {
            patcher_mounts.push(VolumeMount {
                name: format!("debug-patch-input-{index}"),
                mount_path: format!("/debug-patch-inputs/{index}"),
                read_only: Some(true),
                ..Default::default()
            });
        }
        let patcher = Container {
            name: format!("debug-binary-patch-{index}"),
            image: target.image.clone(),
            image_pull_policy: target.image_pull_policy.clone(),
            command: Some(vec!["/debug-tools/debug-patcher".to_string()]),
            args: Some(vec![
                "patch-binary".to_string(),
                patch.path.clone(),
                format!("/debug-patches/{output_name}"),
                patch.find_hex.clone(),
                replacement,
                patch.expected_matches.to_string(),
            ]),
            security_context: target.security_context.clone(),
            volume_mounts: Some(patcher_mounts),
            ..Default::default()
        };
        add_generated_init_container(ops, pod, &patcher, policy)?;
        add_binary_patch_mount(ops, pod, target_index, patch, &output_name)?;
    }
    Ok(())
}

fn validate_binary_patch(patch: &BinaryPatchSpec) -> anyhow::Result<()> {
    anyhow::ensure!(
        patch.path.starts_with('/'),
        "binary patch path must be absolute"
    );
    anyhow::ensure!(
        patch.expected_matches > 0,
        "expectedMatches must be greater than zero"
    );
    let find = hex::decode(
        patch
            .find_hex
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>(),
    )
    .context("binary patch findHex is invalid")?;
    anyhow::ensure!(!find.is_empty(), "binary patch findHex must not be empty");
    match (&patch.replace_hex, &patch.replace_from) {
        (Some(replacement), None) => {
            hex::decode(
                replacement
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            )
            .context("binary patch replaceHex is invalid")?;
        }
        (None, Some(source)) => {
            anyhow::ensure!(
                !source.config_map_key_ref.name.is_empty()
                    && !source.config_map_key_ref.key.is_empty(),
                "binary patch replaceFrom ConfigMap name and key must not be empty"
            );
        }
        (Some(_), Some(_)) => bail!("set only one of replaceHex or replaceFrom"),
        (None, None) => bail!("one of replaceHex or replaceFrom is required"),
    }
    Ok(())
}

fn add_annotations(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    profile: &DebugProfile,
    injection: &InjectionSpec,
) -> anyhow::Result<()> {
    let mut annotations = injection.annotations.clone();
    annotations.insert(MUTATED_BY_ANNOTATION.to_string(), OPERATOR_NAME.to_string());
    annotations.insert(PROFILE_ANNOTATION.to_string(), profile.name_any());
    annotations.insert(
        PROFILE_GENERATION_ANNOTATION.to_string(),
        profile.metadata.generation.unwrap_or_default().to_string(),
    );
    annotations.insert(
        MUTATION_HASH_ANNOTATION.to_string(),
        mutation_hash(profile)?,
    );

    if pod.metadata.annotations.is_none() {
        push_add(ops, ["metadata", "annotations"], json!({}));
    }

    for (key, value) in annotations {
        let path = PointerBuf::from_tokens(["metadata", "annotations", &key]);
        let exists = pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|annotations| annotations.contains_key(&key));
        if exists {
            ops.push(PatchOperation::Replace(ReplaceOperation {
                path,
                value: Value::String(value),
            }));
        } else {
            ops.push(PatchOperation::Add(AddOperation {
                path,
                value: Value::String(value),
            }));
        }
    }
    Ok(())
}

fn add_network(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    profile: &DebugProfile,
) -> anyhow::Result<()> {
    let Some(network) = profile.spec.network.as_ref() else {
        return Ok(());
    };

    match network.mode {
        NetworkMode::ExplicitProxy => add_explicit_proxy_network(ops, pod, profile, network),
        NetworkMode::TransparentProxy => {
            bail!("network mode TransparentProxy is declared but not implemented yet")
        }
        NetworkMode::DnsProxy => bail!("network mode DnsProxy is declared but not implemented yet"),
    }
}

fn add_explicit_proxy_network(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    profile: &DebugProfile,
    network: &NetworkSpec,
) -> anyhow::Result<()> {
    let policy = &profile.spec.conflict_policy;
    let proxy_url = format!("http://127.0.0.1:{}", network.proxy.port);
    let mut env = vec![
        EnvVar {
            name: "HTTP_PROXY".to_string(),
            value: Some(proxy_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "HTTPS_PROXY".to_string(),
            value: Some(proxy_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "ALL_PROXY".to_string(),
            value: Some(proxy_url),
            ..Default::default()
        },
    ];

    if !network.intercept.no_proxy.is_empty() {
        env.push(EnvVar {
            name: "NO_PROXY".to_string(),
            value: Some(network.intercept.no_proxy.join(",")),
            ..Default::default()
        });
    }

    let mut volumes = Vec::new();
    let mut app_mounts = Vec::new();
    let mut proxy_mounts = Vec::new();

    if let Some(config_map) = network.proxy.rules_config_map.as_ref() {
        volumes.push(Volume {
            name: proxy_rules_volume_name(network),
            config_map: Some(ConfigMapVolumeSource {
                name: config_map.clone(),
                optional: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        proxy_mounts.push(VolumeMount {
            name: proxy_rules_volume_name(network),
            mount_path: network.proxy.rules_mount_path.clone(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    if network.tls.trust_ca {
        let ca_secret = network
            .tls
            .ca_secret
            .as_ref()
            .context("network.tls.trustCa requires network.tls.caSecret")?;
        let volume_name = proxy_ca_volume_name(network);
        let ca_path = format!(
            "{}/{}",
            network.tls.ca_mount_path.trim_end_matches('/'),
            network.tls.ca_cert_file
        );
        volumes.push(Volume {
            name: volume_name.clone(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(ca_secret.clone()),
                optional: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        app_mounts.push(VolumeMount {
            name: volume_name.clone(),
            mount_path: network.tls.ca_mount_path.clone(),
            read_only: Some(true),
            ..Default::default()
        });
        proxy_mounts.push(VolumeMount {
            name: volume_name,
            mount_path: network.tls.ca_mount_path.clone(),
            read_only: Some(true),
            ..Default::default()
        });
        for name in &network.tls.env_names {
            env.push(EnvVar {
                name: name.clone(),
                value: Some(ca_path.clone()),
                ..Default::default()
            });
        }
    }

    add_volumes_from_slice(ops, pod, &volumes, policy)?;
    add_volume_mounts_to_app_containers(ops, pod, &app_mounts, policy)?;
    add_env_to_app_containers(ops, pod, &env, policy)?;
    add_proxy_sidecar(ops, pod, network, proxy_mounts, policy)?;
    Ok(())
}

fn add_proxy_sidecar(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    network: &NetworkSpec,
    mounts: Vec<VolumeMount>,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    let mut args = vec![
        "--mode".to_string(),
        "regular".to_string(),
        "--listen-host".to_string(),
        "127.0.0.1".to_string(),
        "--listen-port".to_string(),
        network.proxy.port.to_string(),
    ];
    if network.proxy.rules_config_map.is_some() {
        args.extend([
            "-s".to_string(),
            format!(
                "{}/addon.py",
                network.proxy.rules_mount_path.trim_end_matches('/')
            ),
        ]);
    }
    args.extend(network.proxy.extra_args.clone());

    let sidecar = Container {
        name: network.proxy.name.clone(),
        image: Some(network.proxy.image.clone()),
        args: Some(args),
        volume_mounts: (!mounts.is_empty()).then_some(mounts),
        ..Default::default()
    };

    let existing = pod
        .spec
        .as_ref()
        .map(|spec| {
            spec.containers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let sidecars = [sidecar];
    let additions = filtered_named_items(&sidecars, existing, policy, "container")?;
    if additions.is_empty() {
        return Ok(());
    }
    push_add(
        ops,
        ["spec", "containers", "-"],
        serde_json::to_value(additions[0])?,
    );
    Ok(())
}

fn add_env(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    injection: &InjectionSpec,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if injection.env.is_empty() {
        return Ok(());
    }
    let Some(spec) = pod.spec.as_ref() else {
        bail!("pod has no spec");
    };

    for (container_index, container) in spec.containers.iter().enumerate() {
        let existing = container
            .env
            .as_ref()
            .map(|env| env.iter().map(|e| e.name.as_str()).collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let additions = filtered_named_items(&injection.env, existing, policy, "env")?;
        if additions.is_empty() {
            continue;
        }
        if container.env.is_none() {
            push_add(
                ops,
                ["spec", "containers", &container_index.to_string(), "env"],
                json!([]),
            );
        }
        for env in additions {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "env",
                    "-",
                ],
                serde_json::to_value(env)?,
            );
        }
    }
    Ok(())
}

fn add_env_to_app_containers(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    env: &[EnvVar],
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    let Some(spec) = pod.spec.as_ref() else {
        bail!("pod has no spec");
    };

    for (container_index, container) in spec.containers.iter().enumerate() {
        let existing = container
            .env
            .as_ref()
            .map(|env| env.iter().map(|e| e.name.as_str()).collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let additions = filtered_named_items(env, existing, policy, "env")?;
        if additions.is_empty() {
            continue;
        }
        if container.env.is_none() {
            push_add(
                ops,
                ["spec", "containers", &container_index.to_string(), "env"],
                json!([]),
            );
        }
        for env in additions {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "env",
                    "-",
                ],
                serde_json::to_value(env)?,
            );
        }
    }
    Ok(())
}

fn add_init_containers(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    injection: &InjectionSpec,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if injection.init_containers.is_empty() {
        return Ok(());
    }
    let existing = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .map(|items| {
            items
                .iter()
                .map(|c| c.name.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let additions = filtered_named_items(
        &injection.init_containers,
        existing,
        policy,
        "initContainer",
    )?;
    if additions.is_empty() {
        return Ok(());
    }
    if pod
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .is_none()
    {
        push_add(ops, ["spec", "initContainers"], json!([]));
    }
    for container in additions {
        push_add(
            ops,
            ["spec", "initContainers", "-"],
            serde_json::to_value(container)?,
        );
    }
    Ok(())
}

fn add_generated_init_container(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    container: &Container,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    let exists = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .is_some_and(|containers| containers.iter().any(|item| item.name == container.name));
    if exists {
        return match policy {
            ConflictPolicy::Fail => bail!("initContainer '{}' already exists", container.name),
            ConflictPolicy::Override => Ok(()),
        };
    }
    ensure_array(
        ops,
        pod.spec
            .as_ref()
            .and_then(|spec| spec.init_containers.as_ref())
            .is_some(),
        ["spec", "initContainers"],
    );
    push_add(
        ops,
        ["spec", "initContainers", "-"],
        serde_json::to_value(container)?,
    );
    Ok(())
}

fn add_binary_patch_mount(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    container_index: usize,
    patch: &BinaryPatchSpec,
    output_name: &str,
) -> anyhow::Result<()> {
    let container = &pod.spec.as_ref().context("pod has no spec")?.containers[container_index];
    if container
        .volume_mounts
        .as_ref()
        .is_some_and(|mounts| mounts.iter().any(|mount| mount.mount_path == patch.path))
    {
        bail!(
            "container '{}' already has a volume mounted at binary patch path '{}'",
            patch.container,
            patch.path
        );
    }
    let index = container_index.to_string();
    ensure_array(
        ops,
        container.volume_mounts.is_some(),
        ["spec", "containers", index.as_str(), "volumeMounts"],
    );
    let mount = VolumeMount {
        name: PATCH_OUTPUT_VOLUME.to_string(),
        mount_path: patch.path.clone(),
        read_only: Some(true),
        sub_path: Some(output_name.to_string()),
        ..Default::default()
    };
    push_add(
        ops,
        ["spec", "containers", index.as_str(), "volumeMounts", "-"],
        serde_json::to_value(mount)?,
    );
    Ok(())
}

fn add_volumes(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    injection: &InjectionSpec,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if injection.volumes.is_empty() {
        return Ok(());
    }
    let existing = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .map(|items| {
            items
                .iter()
                .map(|v| v.name.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let additions = filtered_named_items(&injection.volumes, existing, policy, "volume")?;
    if additions.is_empty() {
        return Ok(());
    }
    if pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_none()
    {
        ensure_array(ops, false, ["spec", "volumes"]);
    }
    for volume in additions {
        push_add(ops, ["spec", "volumes", "-"], serde_json::to_value(volume)?);
    }
    Ok(())
}

fn add_volumes_from_slice(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    volumes: &[Volume],
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if volumes.is_empty() {
        return Ok(());
    }
    let existing = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .map(|items| {
            items
                .iter()
                .map(|v| v.name.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let additions = filtered_named_items(volumes, existing, policy, "volume")?;
    if additions.is_empty() {
        return Ok(());
    }
    if pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_none()
    {
        ensure_array(ops, false, ["spec", "volumes"]);
    }
    for volume in additions {
        push_add(ops, ["spec", "volumes", "-"], serde_json::to_value(volume)?);
    }
    Ok(())
}

fn add_volume_mounts(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    injection: &InjectionSpec,
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if injection.volume_mounts.is_empty() {
        return Ok(());
    }
    let Some(spec) = pod.spec.as_ref() else {
        bail!("pod has no spec");
    };

    for (container_index, container) in spec.containers.iter().enumerate() {
        let existing = container
            .volume_mounts
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let additions =
            filtered_named_items(&injection.volume_mounts, existing, policy, "volumeMount")?;
        if additions.is_empty() {
            continue;
        }
        if container.volume_mounts.is_none() {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "volumeMounts",
                ],
                json!([]),
            );
        }
        for mount in additions {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "volumeMounts",
                    "-",
                ],
                serde_json::to_value(mount)?,
            );
        }
    }
    Ok(())
}

fn add_volume_mounts_to_app_containers(
    ops: &mut Vec<PatchOperation>,
    pod: &Pod,
    mounts: &[VolumeMount],
    policy: &ConflictPolicy,
) -> anyhow::Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    let Some(spec) = pod.spec.as_ref() else {
        bail!("pod has no spec");
    };

    for (container_index, container) in spec.containers.iter().enumerate() {
        let existing = container
            .volume_mounts
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let additions = filtered_named_items(mounts, existing, policy, "volumeMount")?;
        if additions.is_empty() {
            continue;
        }
        if container.volume_mounts.is_none() {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "volumeMounts",
                ],
                json!([]),
            );
        }
        for mount in additions {
            push_add(
                ops,
                [
                    "spec",
                    "containers",
                    &container_index.to_string(),
                    "volumeMounts",
                    "-",
                ],
                serde_json::to_value(mount)?,
            );
        }
    }
    Ok(())
}

fn filtered_named_items<'a, T: Named>(
    requested: &'a [T],
    existing: BTreeSet<&str>,
    policy: &ConflictPolicy,
    kind: &str,
) -> anyhow::Result<Vec<&'a T>> {
    let mut out = Vec::new();
    for item in requested {
        if existing.contains(item.name()) {
            match policy {
                ConflictPolicy::Fail => bail!("{kind} '{}' already exists", item.name()),
                ConflictPolicy::Override => continue,
            }
        }
        out.push(item);
    }
    Ok(out)
}

trait Named {
    fn name(&self) -> &str;
}

impl Named for k8s_openapi::api::core::v1::EnvVar {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for k8s_openapi::api::core::v1::Container {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for k8s_openapi::api::core::v1::Volume {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for k8s_openapi::api::core::v1::VolumeMount {
    fn name(&self) -> &str {
        &self.name
    }
}

fn labels_match(required: &BTreeMap<String, String>, actual: &BTreeMap<String, String>) -> bool {
    required
        .iter()
        .all(|(key, value)| actual.get(key).is_some_and(|actual| actual == value))
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    if !value.starts_with(parts[0]) {
        return false;
    }
    if !value.ends_with(parts[parts.len() - 1]) {
        return false;
    }
    let mut remainder = &value[parts[0].len()..];
    for part in &parts[1..parts.len() - 1] {
        if let Some(index) = remainder.find(part) {
            remainder = &remainder[index + part.len()..];
        } else {
            return false;
        }
    }
    true
}

fn push_add<const N: usize>(ops: &mut Vec<PatchOperation>, tokens: [&str; N], value: Value) {
    ops.push(PatchOperation::Add(AddOperation {
        path: PointerBuf::from_tokens(tokens),
        value,
    }));
}

fn ensure_array<const N: usize>(
    ops: &mut Vec<PatchOperation>,
    exists_in_pod: bool,
    tokens: [&str; N],
) {
    if exists_in_pod {
        return;
    }
    let path = PointerBuf::from_tokens(tokens);
    let already_added = ops
        .iter()
        .any(|operation| matches!(operation, PatchOperation::Add(add) if add.path == path));
    if !already_added {
        ops.push(PatchOperation::Add(AddOperation {
            path,
            value: json!([]),
        }));
    }
}

fn mutation_hash(profile: &DebugProfile) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "injection": profile.spec.injection,
        "network": profile.spec.network,
    }))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}")[..16].to_string())
}

fn proxy_rules_volume_name(network: &NetworkSpec) -> String {
    format!("{}-rules", network.proxy.name)
}

fn proxy_ca_volume_name(network: &NetworkSpec) -> String {
    format!("{}-ca", network.proxy.name)
}

fn already_mutated(pod: &Pod) -> bool {
    pod.metadata
        .annotations
        .as_ref()
        .is_some_and(|annotations| annotations.contains_key(MUTATED_BY_ANNOTATION))
}

fn is_operator_pod(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(MANAGED_BY_LABEL))
        .is_some_and(|value| value == OPERATOR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        BinaryPatchReplaceSource, BinaryPatchSpec, DebugProfileSpec, NetworkSpec, ProfileSelector,
        ProxySpec, TlsMitmSpec,
    };
    use k8s_openapi::api::core::v1::ConfigMapKeySelector;
    use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec};
    use kube::core::ObjectMeta;

    fn profile() -> DebugProfile {
        DebugProfile::new(
            "python-debug",
            DebugProfileSpec {
                selector: ProfileSelector {
                    namespace_opt_in: BTreeMap::from([(
                        "debug-operator.hadron.re/enabled".to_string(),
                        "true".to_string(),
                    )]),
                    pod_labels: BTreeMap::from([("app".to_string(), "target".to_string())]),
                    helm_releases: vec![],
                    image_globs: vec!["ghcr.io/acme/*".to_string()],
                },
                bootstrap: None,
                network: None,
                injection: InjectionSpec {
                    env: vec![EnvVar {
                        name: "DEBUG_ENABLED".to_string(),
                        value: Some("true".to_string()),
                        ..Default::default()
                    }],
                    init_containers: vec![Container {
                        name: "debug-init".to_string(),
                        image: Some("python:3.13-alpine".to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                conflict_policy: ConflictPolicy::Fail,
            },
        )
    }

    fn pod() -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("target".to_string()),
                namespace: Some("research".to_string()),
                labels: Some(BTreeMap::from([("app".to_string(), "target".to_string())])),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "app".to_string(),
                    image: Some("ghcr.io/acme/api:latest".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn matches_namespace_labels_pod_labels_and_image_glob() {
        let ns_labels = BTreeMap::from([(
            "debug-operator.hadron.re/enabled".to_string(),
            "true".to_string(),
        )]);
        assert!(profile_matches(&profile(), &pod(), &ns_labels));
    }

    #[test]
    fn rejects_unlabelled_namespace() {
        assert!(!profile_matches(&profile(), &pod(), &BTreeMap::new()));
    }

    #[test]
    fn builds_idempotent_json_patch() {
        let patch = build_patch(&profile(), &pod()).unwrap();
        let paths = patch
            .0
            .iter()
            .map(|op| match op {
                PatchOperation::Add(op) => op.path.to_string(),
                PatchOperation::Replace(op) => op.path.to_string(),
                other => panic!("unexpected op {other:?}"),
            })
            .collect::<Vec<_>>();

        assert!(paths.contains(&"/metadata/annotations".to_string()));
        assert!(paths.contains(&"/spec/containers/0/env".to_string()));
        assert!(paths.contains(&"/spec/containers/0/env/-".to_string()));
        assert!(paths.contains(&"/spec/initContainers".to_string()));
        assert!(paths.contains(&"/spec/initContainers/-".to_string()));
    }

    #[test]
    fn fails_on_conflicting_env_by_default() {
        let mut pod = pod();
        pod.spec.as_mut().unwrap().containers[0].env = Some(vec![EnvVar {
            name: "DEBUG_ENABLED".to_string(),
            value: Some("old".to_string()),
            ..Default::default()
        }]);
        let err = build_patch(&profile(), &pod).unwrap_err();
        assert!(
            err.to_string()
                .contains("env 'DEBUG_ENABLED' already exists")
        );
    }

    #[test]
    fn explicit_proxy_network_injects_sidecar_env_and_ca() {
        let mut profile = profile();
        profile.spec.network = Some(NetworkSpec {
            proxy: ProxySpec {
                rules_config_map: Some("debug-proxy-rules".to_string()),
                ..Default::default()
            },
            tls: TlsMitmSpec {
                trust_ca: true,
                ca_secret: Some("debug-mitm-ca".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });

        let patch = build_patch(&profile, &pod()).unwrap();
        let rendered = serde_json::to_string(&patch).unwrap();

        assert!(rendered.contains("HTTP_PROXY"));
        assert!(rendered.contains("debug-mitm-proxy"));
        assert!(rendered.contains("debug-mitm-ca"));
        assert!(rendered.contains("debug-proxy-rules"));
    }

    #[test]
    fn binary_patch_injects_patcher_chain_and_subpath_mount() {
        let mut profile = profile();
        profile.spec.injection.binary_patches = vec![BinaryPatchSpec {
            container: "app".to_string(),
            path: "/usr/local/bin/service".to_string(),
            find_hex: "48 89 e5".to_string(),
            replace_hex: Some("90 90 90".to_string()),
            replace_from: None,
            expected_matches: 1,
            patcher_image: Some("example.test/debug-operator:1.0".to_string()),
        }];

        let patch = build_patch(&profile, &pod()).unwrap();
        let mut rendered_pod = serde_json::to_value(pod()).unwrap();
        json_patch::patch(&mut rendered_pod, &patch).unwrap();

        let init_containers = rendered_pod["spec"]["initContainers"].as_array().unwrap();
        assert!(init_containers.iter().any(|container| {
            container["name"] == PATCH_INSTALLER
                && container["image"] == "example.test/debug-operator:1.0"
        }));
        assert!(
            init_containers
                .iter()
                .any(|container| container["name"] == "debug-binary-patch-0")
        );
        let mounts = rendered_pod["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert!(mounts.iter().any(|mount| {
            mount["mountPath"] == "/usr/local/bin/service" && mount["subPath"] == "patch-0"
        }));
    }

    #[test]
    fn binary_patch_rejects_invalid_hex_during_admission() {
        let mut profile = profile();
        profile.spec.injection.binary_patches = vec![BinaryPatchSpec {
            container: "app".to_string(),
            path: "/some/path".to_string(),
            find_hex: "not hex".to_string(),
            replace_hex: Some("00".to_string()),
            replace_from: None,
            expected_matches: 1,
            patcher_image: None,
        }];

        assert!(
            build_patch(&profile, &pod())
                .unwrap_err()
                .to_string()
                .contains("findHex")
        );
    }

    #[test]
    fn binary_patch_mounts_replacement_from_config_map() {
        let mut profile = profile();
        profile.spec.injection.binary_patches = vec![BinaryPatchSpec {
            container: "app".to_string(),
            path: "/some/path".to_string(),
            find_hex: "aabb".to_string(),
            replace_hex: None,
            replace_from: Some(BinaryPatchReplaceSource {
                config_map_key_ref: ConfigMapKeySelector {
                    name: "bootstrap-patch".to_string(),
                    key: "replacement.hex".to_string(),
                    ..Default::default()
                },
            }),
            expected_matches: 1,
            patcher_image: None,
        }];

        let patch = build_patch(&profile, &pod()).unwrap();
        let rendered = serde_json::to_string(&patch).unwrap();
        assert!(rendered.contains("bootstrap-patch"));
        assert!(rendered.contains("replacement.hex"));
        assert!(rendered.contains("@/debug-patch-inputs/0/value"));
    }
}
