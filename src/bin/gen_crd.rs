use valkey_ai_operator::crd::ValkeyCluster;
use kube::CustomResourceExt;

fn main() {
    let crd = ValkeyCluster::crd();
    print!("{}", serde_yaml::to_string(&crd).unwrap());
}
