use pilotis_operator::crd::AIResource;
use kube::CustomResourceExt;

fn main() {
    print!("{}", serde_yaml::to_string(&AIResource::crd()).unwrap());
}
