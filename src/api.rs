use std::collections::BTreeMap;

use k8s_openapi::api::{
    batch::v1::JobSpec,
    core::v1::{Container, EnvVar, Volume, VolumeMount},
};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const NS_OPT_IN_LABEL: &str = "debug-operator.hadron.re/enabled";
pub const MUTATED_BY_ANNOTATION: &str = "debug-operator.hadron.re/mutated";
pub const PROFILE_ANNOTATION: &str = "debug-operator.hadron.re/profile";
pub const PROFILE_GENERATION_ANNOTATION: &str = "debug-operator.hadron.re/profile-generation";
pub const MUTATION_HASH_ANNOTATION: &str = "debug-operator.hadron.re/mutation-hash";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const OPERATOR_NAME: &str = "debug-operator";

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "debug.hadron.re",
    version = "v1alpha1",
    kind = "DebugProfile",
    plural = "debugprofiles",
    namespaced,
    status = "DebugProfileStatus",
    shortname = "dbgprof"
)]
#[serde(rename_all = "camelCase")]
pub struct DebugProfileSpec {
    pub selector: ProfileSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapSpec>,
    #[serde(default)]
    pub injection: InjectionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSpec>,
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSelector {
    #[serde(default = "default_namespace_opt_in")]
    pub namespace_opt_in: BTreeMap<String, String>,
    #[serde(default)]
    pub pod_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub helm_releases: Vec<String>,
    #[serde(default)]
    pub image_globs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSpec {
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<i64>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub output_config_maps: Vec<String>,
    #[serde(default)]
    pub output_secrets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_template: Option<JobSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InjectionSpec {
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub init_containers: Vec<Container>,
    #[serde(default)]
    pub volumes: Vec<Volume>,
    #[serde(default)]
    pub volume_mounts: Vec<VolumeMount>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    #[serde(default)]
    pub mode: NetworkMode,
    #[serde(default)]
    pub proxy: ProxySpec,
    #[serde(default)]
    pub tls: TlsMitmSpec,
    #[serde(default)]
    pub intercept: InterceptSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsInterceptionSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
    #[default]
    ExplicitProxy,
    TransparentProxy,
    DnsProxy,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxySpec {
    #[serde(default = "default_proxy_name")]
    pub name: String,
    #[serde(default = "default_proxy_image")]
    pub image: String,
    #[serde(default = "default_proxy_port")]
    pub port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_config_map: Option<String>,
    #[serde(default = "default_proxy_rules_mount_path")]
    pub rules_mount_path: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TlsMitmSpec {
    #[serde(default)]
    pub trust_ca: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_secret: Option<String>,
    #[serde(default = "default_ca_mount_path")]
    pub ca_mount_path: String,
    #[serde(default = "default_ca_cert_file")]
    pub ca_cert_file: String,
    #[serde(default = "default_ca_env_names")]
    pub env_names: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InterceptSpec {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default = "default_intercept_ports")]
    pub ports: Vec<i32>,
    #[serde(default)]
    pub no_proxy: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DnsInterceptionSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_dns_port")]
    pub port: i32,
}

impl Default for ProxySpec {
    fn default() -> Self {
        Self {
            name: default_proxy_name(),
            image: default_proxy_image(),
            port: default_proxy_port(),
            rules_config_map: None,
            rules_mount_path: default_proxy_rules_mount_path(),
            extra_args: Vec::new(),
        }
    }
}

impl Default for TlsMitmSpec {
    fn default() -> Self {
        Self {
            trust_ca: false,
            ca_secret: None,
            ca_mount_path: default_ca_mount_path(),
            ca_cert_file: default_ca_cert_file(),
            env_names: default_ca_env_names(),
        }
    }
}

impl Default for DnsInterceptionSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_dns_port(),
        }
    }
}

impl Default for NetworkSpec {
    fn default() -> Self {
        Self {
            mode: NetworkMode::ExplicitProxy,
            proxy: ProxySpec::default(),
            tls: TlsMitmSpec::default(),
            intercept: InterceptSpec::default(),
            dns: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    #[default]
    Fail,
    Override,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DebugProfileStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub bootstrap_job: Option<String>,
    #[serde(default)]
    pub output_config_maps: Vec<String>,
    #[serde(default)]
    pub output_secrets: Vec<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    pub profiles: Vec<DebugProfile>,
}

fn default_namespace_opt_in() -> BTreeMap<String, String> {
    BTreeMap::from([(NS_OPT_IN_LABEL.to_string(), "true".to_string())])
}

fn default_proxy_name() -> String {
    "debug-mitm-proxy".to_string()
}

fn default_proxy_image() -> String {
    "mitmproxy/mitmproxy:latest".to_string()
}

fn default_proxy_port() -> i32 {
    15080
}

fn default_proxy_rules_mount_path() -> String {
    "/debug-proxy/rules".to_string()
}

fn default_ca_mount_path() -> String {
    "/debug-proxy/ca".to_string()
}

fn default_ca_cert_file() -> String {
    "mitmproxy-ca-cert.pem".to_string()
}

fn default_ca_env_names() -> Vec<String> {
    vec![
        "SSL_CERT_FILE".to_string(),
        "REQUESTS_CA_BUNDLE".to_string(),
        "NODE_EXTRA_CA_CERTS".to_string(),
    ]
}

fn default_intercept_ports() -> Vec<i32> {
    vec![80, 443]
}

fn default_dns_port() -> i32 {
    15053
}
