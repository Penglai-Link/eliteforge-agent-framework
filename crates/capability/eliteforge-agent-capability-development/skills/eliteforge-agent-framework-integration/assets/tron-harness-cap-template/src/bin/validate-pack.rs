//! Validate an archive using the same installer as Tron, without activating its code.
use eliteforge_agent_capability_runtime::{CapabilityPackManager, InstallPolicy};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<_> = std::env::args_os().skip(1).collect();
    if paths.is_empty() {
        return Err("usage: validate-pack <archive.zip> [...]".into());
    }
    let directory = tempfile::tempdir()?;
    let manager = CapabilityPackManager::new(directory.path())?;
    for path in paths {
        let pack = manager.install_file(std::path::Path::new(&path), InstallPolicy::Development)?;
        println!(
            "{} {}: archive accepted",
            pack.manifest.id, pack.manifest.version
        );
    }
    Ok(())
}
