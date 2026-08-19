use std::{collections::BTreeMap, sync::Arc, time::Duration};

use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{ConfigMap, Container, EnvVar, Namespace, Pod, PodSpec, PodTemplateSpec},
};
use kube::{
    Client, Resource, ResourceExt,
    api::{Api, ListParams, LogParams, Patch, PatchParams, PostParams},
    runtime::watcher,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::api::{
    BootstrapSpec, DebugProfile, DebugProfileStatus, MANAGED_BY_LABEL, OPERATOR_NAME, ProfileStore,
};

const OUTPUT_MARKER: &str = "DEBUG_OPERATOR_OUTPUTS=";

pub async fn run_profile_cache(client: Client, store: Arc<RwLock<ProfileStore>>) {
    let profiles: Api<DebugProfile> = Api::all(client);
    let mut stream = watcher(profiles, watcher::Config::default()).boxed();

    loop {
        match stream.try_next().await {
            Ok(Some(event)) => {
                let mut guard = store.write().await;
                match event {
                    watcher::Event::Apply(profile) | watcher::Event::InitApply(profile) => {
                        upsert(&mut guard.profiles, profile);
                    }
                    watcher::Event::Delete(profile) => {
                        let uid = profile.metadata.uid.clone();
                        guard.profiles.retain(|p| p.metadata.uid != uid);
                    }
                    watcher::Event::Init | watcher::Event::InitDone => {
                        if matches!(event, watcher::Event::Init) {
                            guard.profiles.clear();
                        }
                    }
                }
            }
            Ok(None) => {
                warn!("profile watcher ended");
                break;
            }
            Err(err) => {
                error!(?err, "profile watcher error");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

pub async fn run_bootstrap_controller(client: Client) {
    let profiles: Api<DebugProfile> = Api::all(client.clone());
    let mut interval = tokio::time::interval(Duration::from_secs(15));

    loop {
        interval.tick().await;
        match profiles.list(&Default::default()).await {
            Ok(list) => {
                for profile in list {
                    if let Err(err) = reconcile_profile(client.clone(), profile).await {
                        error!(?err, "failed to reconcile profile bootstrap");
                    }
                }
            }
            Err(err) => error!(?err, "failed to list profiles for bootstrap reconciliation"),
        }
    }
}

async fn reconcile_profile(client: Client, profile: DebugProfile) -> anyhow::Result<()> {
    let ns = profile
        .namespace()
        .context("DebugProfile must be namespaced")?;
    let name = profile.name_any();
    let generation = profile.metadata.generation.unwrap_or_default();

    let Some(bootstrap) = profile.spec.bootstrap.as_ref() else {
        patch_status(
            &client,
            &ns,
            &name,
            DebugProfileStatus {
                observed_generation: Some(generation),
                ready: true,
                ..Default::default()
            },
        )
        .await?;
        return Ok(());
    };

    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let job_name = bootstrap_job_name(&name, generation);
    let job = if let Some(job) = jobs.get_opt(&job_name).await? {
        job
    } else {
        let job = build_bootstrap_job(&profile, bootstrap, &job_name)?;
        let job = jobs.create(&PostParams::default(), &job).await?;
        info!(profile = %name, job = %job_name, "created bootstrap job");
        job
    };

    let failed = job
        .status
        .as_ref()
        .and_then(|status| status.failed)
        .unwrap_or_default()
        > 0;
    let succeeded = job
        .status
        .as_ref()
        .and_then(|status| status.succeeded)
        .unwrap_or_default()
        > 0;

    let (ready, output_config_maps, last_error) = if succeeded {
        match publish_config_maps(&client, &profile, &job_name, bootstrap).await {
            Ok(names) => (true, names, None),
            Err(err) => {
                warn!(profile = %name, job = %job_name, ?err, "failed to publish bootstrap outputs");
                (false, Vec::new(), Some(err.to_string()))
            }
        }
    } else if failed {
        (false, Vec::new(), Some("bootstrap job failed".to_string()))
    } else {
        (false, Vec::new(), None)
    };
    patch_status(
        &client,
        &ns,
        &name,
        DebugProfileStatus {
            observed_generation: Some(generation),
            ready: ready || bootstrap.optional,
            bootstrap_job: Some(job_name),
            output_config_maps,
            output_secrets: Vec::new(),
            last_error,
        },
    )
    .await?;
    Ok(())
}

fn build_bootstrap_job(
    profile: &DebugProfile,
    bootstrap: &BootstrapSpec,
    job_name: &str,
) -> anyhow::Result<Job> {
    if let Some(template) = bootstrap.job_template.clone() {
        return Ok(Job {
            metadata: kube::core::ObjectMeta {
                name: Some(job_name.to_string()),
                namespace: profile.namespace(),
                labels: Some(std::collections::BTreeMap::from([(
                    MANAGED_BY_LABEL.to_string(),
                    OPERATOR_NAME.to_string(),
                )])),
                owner_references: profile.controller_owner_ref(&()).map(|owner| vec![owner]),
                ..Default::default()
            },
            spec: Some(template),
            ..Default::default()
        });
    }

    let wrapper = bootstrap_wrapper(bootstrap)?;
    let mut env = bootstrap.env.clone();
    env.retain(|variable| variable.name != "BOOTSTRAP_OUTPUT_DIRECTORY");
    env.push(EnvVar {
        name: "BOOTSTRAP_OUTPUT_DIRECTORY".to_string(),
        value: Some(bootstrap.output_directory.clone()),
        ..Default::default()
    });

    Ok(Job {
        metadata: kube::core::ObjectMeta {
            name: Some(job_name.to_string()),
            namespace: profile.namespace(),
            labels: Some(std::collections::BTreeMap::from([(
                MANAGED_BY_LABEL.to_string(),
                OPERATOR_NAME.to_string(),
            )])),
            owner_references: profile.controller_owner_ref(&()).map(|owner| vec![owner]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            active_deadline_seconds: bootstrap.timeout_seconds,
            backoff_limit: Some(0),
            template: PodTemplateSpec {
                metadata: Some(kube::core::ObjectMeta {
                    labels: Some(std::collections::BTreeMap::from([(
                        MANAGED_BY_LABEL.to_string(),
                        OPERATOR_NAME.to_string(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    service_account_name: bootstrap.service_account_name.clone(),
                    containers: vec![Container {
                        name: "bootstrap".to_string(),
                        image: Some(bootstrap.image.clone()),
                        command: Some(vec!["python".to_string(), "-c".to_string()]),
                        args: Some(vec![wrapper]),
                        env: Some(env),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn bootstrap_wrapper(bootstrap: &BootstrapSpec) -> anyhow::Result<String> {
    let dependencies = serde_json::to_string(&bootstrap.python_dependencies)?;
    let source = bootstrap
        .source
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_else(|| "None".to_string());
    let command = serde_json::to_string(&bootstrap.command)?;
    let args = serde_json::to_string(&bootstrap.args)?;
    let output_directory = serde_json::to_string(&bootstrap.output_directory)?;

    Ok(format!(
        r#"import base64, json, os, pathlib, subprocess, sys
dependencies = {dependencies}
source = {source}
command = {command}
args = {args}
output_directory = {output_directory}
if dependencies:
    subprocess.run([sys.executable, "-m", "pip", "install", *dependencies], check=True)
if source is not None:
    exec(compile(source, "<bootstrap>", "exec"), {{"__name__": "__main__"}})
elif command:
    subprocess.run([*command, *args], check=True)
root = pathlib.Path(output_directory) / "configmaps"
outputs = {{}}
if root.exists():
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        relative = path.relative_to(root)
        if len(relative.parts) != 2:
            raise ValueError(f"output path must be configmaps/<name>/<key>: {{relative}}")
        outputs[str(relative)] = base64.b64encode(path.read_bytes()).decode("ascii")
print("{OUTPUT_MARKER}" + json.dumps(outputs, separators=(",", ":")))
"#
    ))
}

async fn publish_config_maps(
    client: &Client,
    profile: &DebugProfile,
    job_name: &str,
    bootstrap: &BootstrapSpec,
) -> anyhow::Result<Vec<String>> {
    let ns = profile
        .namespace()
        .context("DebugProfile must be namespaced")?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let pod_list = pods
        .list(&ListParams::default().labels(&format!("job-name={job_name}")))
        .await?;
    anyhow::ensure!(!pod_list.items.is_empty(), "bootstrap pod not found");
    let mut payload = None;
    let mut log_errors = Vec::new();
    for pod in pod_list.items.iter().rev() {
        match pods
            .logs(
                &pod.name_any(),
                &LogParams {
                    container: Some("bootstrap".to_string()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(logs) => {
                if let Some(record) = logs
                    .lines()
                    .rev()
                    .find_map(|line| line.strip_prefix(OUTPUT_MARKER))
                {
                    payload = Some(record.to_string());
                    break;
                }
            }
            Err(err) => log_errors.push(format!("{}: {err}", pod.name_any())),
        }
    }
    let payload = payload.with_context(|| {
        format!(
            "bootstrap output record not found in pod logs{}",
            if log_errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", log_errors.join(", "))
            }
        )
    })?;
    let files: BTreeMap<String, String> = serde_json::from_str(&payload)?;
    let mut maps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut binary_maps: BTreeMap<String, BTreeMap<String, k8s_openapi::ByteString>> =
        BTreeMap::new();
    for (relative, encoded) in files {
        let (map_name, key) = relative
            .split_once('/')
            .context("bootstrap output path must be <configmap>/<key>")?;
        let value = BASE64.decode(encoded)?;
        match String::from_utf8(value) {
            Ok(value) => {
                maps.entry(map_name.to_string())
                    .or_default()
                    .insert(key.to_string(), value);
            }
            Err(err) => {
                binary_maps
                    .entry(map_name.to_string())
                    .or_default()
                    .insert(key.to_string(), k8s_openapi::ByteString(err.into_bytes()));
            }
        }
    }

    let declared = &bootstrap.output_config_maps;
    if !declared.is_empty() {
        for name in maps.keys().chain(binary_maps.keys()) {
            anyhow::ensure!(
                declared.contains(name),
                "undeclared output ConfigMap {name}"
            );
        }
        for name in declared {
            anyhow::ensure!(
                maps.contains_key(name) || binary_maps.contains_key(name),
                "declared output ConfigMap {name} has no files"
            );
        }
    }

    let names: std::collections::BTreeSet<_> =
        maps.keys().chain(binary_maps.keys()).cloned().collect();

    let namespaces: Api<Namespace> = Api::all(client.clone());
    let mut target_namespaces: std::collections::BTreeSet<String> = namespaces
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|namespace| {
            labels_match(
                &profile.spec.selector.namespace_opt_in,
                namespace.metadata.labels.as_ref(),
            )
        })
        .filter_map(|namespace| namespace.metadata.name)
        .collect();
    target_namespaces.insert(ns.clone());

    for target_ns in target_namespaces {
        let config_maps: Api<ConfigMap> = Api::namespaced(client.clone(), &target_ns);
        for map_name in &names {
            let config_map = ConfigMap {
                metadata: kube::core::ObjectMeta {
                    name: Some(map_name.clone()),
                    namespace: Some(target_ns.clone()),
                    labels: Some(BTreeMap::from([(
                        MANAGED_BY_LABEL.to_string(),
                        OPERATOR_NAME.to_string(),
                    )])),
                    annotations: Some(BTreeMap::from([
                        (
                            "debug-operator.hadron.re/source-profile".to_string(),
                            format!("{ns}/{}", profile.name_any()),
                        ),
                        (
                            "debug-operator.hadron.re/source-profile-uid".to_string(),
                            profile.metadata.uid.clone().unwrap_or_default(),
                        ),
                    ])),
                    owner_references: (target_ns == ns)
                        .then(|| profile.controller_owner_ref(&()))
                        .flatten()
                        .map(|owner| vec![owner]),
                    ..Default::default()
                },
                data: maps.get(map_name).cloned(),
                binary_data: binary_maps.get(map_name).cloned(),
                ..Default::default()
            };
            config_maps
                .patch(
                    map_name,
                    &PatchParams::apply(OPERATOR_NAME).force(),
                    &Patch::Apply(&config_map),
                )
                .await?;
        }
    }
    Ok(names.into_iter().collect())
}

fn labels_match(
    required: &BTreeMap<String, String>,
    actual: Option<&BTreeMap<String, String>>,
) -> bool {
    required
        .iter()
        .all(|(key, value)| actual.and_then(|labels| labels.get(key)) == Some(value))
}

async fn patch_status(
    client: &Client,
    ns: &str,
    name: &str,
    status: DebugProfileStatus,
) -> anyhow::Result<()> {
    let profiles: Api<DebugProfile> = Api::namespaced(client.clone(), ns);
    profiles
        .patch_status(
            name,
            &PatchParams::apply(OPERATOR_NAME),
            &Patch::Apply(serde_json::json!({
                "apiVersion": "debug.hadron.re/v1alpha1",
                "kind": "DebugProfile",
                "status": status,
            })),
        )
        .await?;
    Ok(())
}

fn upsert(profiles: &mut Vec<DebugProfile>, profile: DebugProfile) {
    let uid = profile.metadata.uid.clone();
    if let Some(existing) = profiles.iter_mut().find(|p| p.metadata.uid == uid) {
        *existing = profile;
    } else {
        profiles.push(profile);
    }
}

fn bootstrap_job_name(profile_name: &str, generation: i64) -> String {
    format!("{profile_name}-bootstrap-g{generation}")
        .chars()
        .take(63)
        .collect()
}
