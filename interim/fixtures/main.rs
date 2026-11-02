// A stand-in for the shape of OpenShell's driver entry point, for testing
// apply-launcher-override.sh. It has the two anchors the script looks for and
// nothing else of OpenShell's.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "openshell-driver-vm")]
struct Args {
    /// Operator-owned JSON policy.
    #[arg(long, env = "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON", default_value = "{}")]
    admission_config_json: String,

    #[arg(long, env = "OPENSHELL_VM_DRIVER_STATE_DIR")]
    state_dir: PathBuf,
}

fn main() {
    let args = Args::parse();
    let _config = VmDriverConfig {
        state_dir: args.state_dir.clone(),
        launcher_bin: None,
        default_image: String::new(),
    };
}
