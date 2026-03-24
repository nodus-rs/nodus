mod discover;
mod init;
mod load;
mod model_impls;
#[cfg(test)]
mod tests;
mod types;

pub use discover::normalize_dependency_alias;
pub use init::{scaffold_init_in_dir, scaffold_init_in_dir_dry_run};
pub use load::{load_dependency_from_dir, load_from_dir, load_root_from_dir, serialize_manifest};
pub use model_impls::RequestedGitRef;
pub use types::{
    AdapterConfig, Capability, DependencyComponent, DependencyEntry, DependencyKind,
    DependencySourceKind, DependencySpec, FileEntry, InitSummary, LaunchHookConfig, LoadedManifest,
    MANIFEST_FILE, ManagedPathSpec, Manifest, McpServerConfig, PackageContents, PackageRole,
    SkillEntry,
};
