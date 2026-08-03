use std::{sync::Arc, time::Duration};

use anyhow::Context;
use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{ConfigMap, Container, PodSpec, PodTemplateSpec, Secret},
};
use kube::{
    Client, Resource, ResourceExt,
    api::{Api, Patch, PatchParams, PostParams},
    runtime::watcher,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::api::{
    BootstrapSpec, DebugProfile, DebugProfileStatus, MANAGED_BY_LABEL, OPERATOR_NAME, ProfileStore,
};

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
    if jobs.get_opt(&job_name).await?.is_none() {
        let job = build_bootstrap_job(&profile, bootstrap, &job_name)?;
        jobs.create(&PostParams::default(), &job).await?;
        info!(profile = %name, job = %job_name, "created bootstrap job");
    }

    let ready = outputs_exist(&client, &ns, bootstrap).await?;
    patch_status(
        &client,
        &ns,
        &name,
        DebugProfileStatus {
            observed_generation: Some(generation),
            ready: ready || bootstrap.optional,
            bootstrap_job: Some(job_name),
            output_config_maps: bootstrap.output_config_maps.clone(),
            output_secrets: bootstrap.output_secrets.clone(),
            last_error: None,
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

    let mut command = bootstrap.command.clone();
    let mut args = bootstrap.args.clone();
    if let Some(source) = bootstrap.source.as_ref() {
        command = vec!["python".to_string(), "-c".to_string()];
        args = vec![source.clone()];
    }

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
                        command: (!command.is_empty()).then_some(command),
                        args: (!args.is_empty()).then_some(args),
                        env: (!bootstrap.env.is_empty()).then_some(bootstrap.env.clone()),
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

async fn outputs_exist(
    client: &Client,
    ns: &str,
    bootstrap: &BootstrapSpec,
) -> anyhow::Result<bool> {
    let config_maps: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    for name in &bootstrap.output_config_maps {
        if config_maps.get_opt(name).await?.is_none() {
            return Ok(false);
        }
    }

    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    for name in &bootstrap.output_secrets {
        if secrets.get_opt(name).await?.is_none() {
            return Ok(false);
        }
    }

    Ok(true)
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
                "apiVersion": "debug.cfi.dev/v1alpha1",
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
