use debug_operator::api::DebugProfile;
use kube::CustomResourceExt;

fn main() -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&DebugProfile::crd())?);
    Ok(())
}
