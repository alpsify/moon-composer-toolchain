use crate::manifest::SyncSettings;
use moon_pdk_api::config_struct;
use schematic::Schematic;

config_struct!(
    /// Configures and enables the Composer toolchain, which keeps the
    /// `composer.json` of each project in sync with its `dependsOn`.
    #[derive(Schematic)]
    pub struct ComposerToolchainConfig {
        /// Version constraint of a workspace package added to `require` or
        /// `require-dev`. The constraint of a package already required is
        /// left as is.
        #[schema(default = "*")]
        pub dependency_version_constraint: String,

        /// Value of `options.symlink` in the `path` repositories. Not written
        /// when omitted, which leaves the choice to Composer.
        pub path_repository_symlink: Option<bool>,

        /// URL of a single `path` repository that holds every workspace
        /// package, relative to each project (`../packages/*`). The packages
        /// are then listed in its `options.versions`. When omitted, each
        /// package gets its own `path` repository, pointing to its project.
        pub path_repository_url: Option<String>,

        /// Version given to the workspace packages in `options.versions` of
        /// the `path` repositories, unless their `composer.json` declares a
        /// `version`. Nothing is written when empty.
        #[schema(default = "0.0.0")]
        pub path_repository_version: String,

        /// Removes from `require`, `require-dev` and `repositories` the
        /// workspace packages that are no longer in `dependsOn`.
        #[schema(default = true)]
        pub prune_workspace_dependencies: bool,

        /// Syncs the `path` repositories of the workspace packages a project
        /// depends on, directly or not, to `repositories`.
        #[schema(default = true)]
        pub sync_path_repositories: bool,

        /// Syncs the `dependsOn` of a project to `require`, or to
        /// `require-dev` for the ones of the `development` scope.
        #[schema(default = true)]
        pub sync_project_workspace_dependencies: bool,
    }
);

impl Default for ComposerToolchainConfig {
    fn default() -> Self {
        Self {
            dependency_version_constraint: "*".into(),
            path_repository_symlink: None,
            path_repository_url: None,
            path_repository_version: "0.0.0".into(),
            prune_workspace_dependencies: true,
            sync_path_repositories: true,
            sync_project_workspace_dependencies: true,
        }
    }
}

impl ComposerToolchainConfig {
    pub fn to_sync_settings(&self) -> SyncSettings {
        let non_empty = |value: &str| {
            let value = value.trim();

            (!value.is_empty()).then(|| value.to_owned())
        };

        SyncSettings {
            sync_requirements: self.sync_project_workspace_dependencies,
            constraint: non_empty(&self.dependency_version_constraint)
                .unwrap_or_else(|| "*".into()),
            prune: self.prune_workspace_dependencies,
            sync_repositories: self.sync_path_repositories,
            repository_url: self.path_repository_url.as_deref().and_then(non_empty),
            repository_symlink: self.path_repository_symlink,
            repository_version: non_empty(&self.path_repository_version),
        }
    }
}
