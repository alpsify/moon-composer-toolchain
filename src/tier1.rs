use crate::config::ComposerToolchainConfig;
use crate::manifest::*;
use extism_pdk::*;
use moon_config::{DependencyScope, LanguageType};
use moon_pdk::{HostLogInput, host_log, is_project_toolchain_enabled, parse_toolchain_config};
use moon_pdk_api::*;
use schematic::SchemaBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::fs;

const MANIFEST: &str = "composer.json";
const LOCK: &str = "composer.lock";

/// Files that make a directory a moon project.
const PROJECT_CONFIGS: [&str; 7] = [
    "moon.yml",
    "moon.yaml",
    "moon.pkl",
    "moon.json",
    "moon.jsonc",
    "moon.toml",
    "moon.hcl",
];

/// The few fields read from a project of the graph. moon serializes the whole
/// project, whose shape changes from one release to the next (moon 2.6 can't
/// be read with the types of moon 2.5): reading only what is needed keeps the
/// plugin working across versions.
#[derive(Deserialize)]
struct GraphProject {
    source: String,
    #[serde(default)]
    dependencies: Vec<GraphDependency>,
}

#[derive(Deserialize)]
struct GraphDependency {
    id: String,
    #[serde(default)]
    scope: Option<String>,
}

impl GraphDependency {
    /// Whether Composer installs it along with the package that requires it,
    /// which it doesn't for the `require-dev` of a dependency.
    fn is_installed_with_dependent(&self) -> bool {
        matches!(self.scope.as_deref(), None | Some("production" | "peer"))
    }
}

#[host_fn]
extern "ExtismHost" {
    fn host_log(input: Json<HostLogInput>);
    fn load_project_by_id(id: String) -> Json<GraphProject>;
}

fn load_graph_project(id: &str) -> AnyResult<GraphProject> {
    let project = unsafe { load_project_by_id(id.into())? };

    Ok(project.0)
}

#[plugin_fn]
pub fn register_toolchain(
    Json(_): Json<RegisterToolchainInput>,
) -> FnResult<Json<RegisterToolchainOutput>> {
    Ok(Json(RegisterToolchainOutput {
        name: "Composer".into(),
        description: Some(
            "Provides sync operations that keep <file>composer.json</file>'s in a healthy state."
                .into(),
        ),
        plugin_version: env!("CARGO_PKG_VERSION").into(),
        // Every PHP project gets the toolchain, and so does any project that
        // has one of the files below. No executable is declared: the plugin
        // only edits manifests, it never runs PHP or Composer.
        language: Some(LanguageType::Php),
        manifest_file_names: vec![MANIFEST.into()],
        lock_file_names: vec![LOCK.into()],
        vendor_dir_name: Some("vendor".into()),
        ..Default::default()
    }))
}

#[plugin_fn]
pub fn define_toolchain_config() -> FnResult<Json<DefineToolchainConfigOutput>> {
    Ok(Json(DefineToolchainConfigOutput {
        schema: SchemaBuilder::build_root::<ComposerToolchainConfig>(),
    }))
}

#[plugin_fn]
pub fn sync_project(Json(input): Json<SyncProjectInput>) -> FnResult<Json<SyncOutput>> {
    let mut output = SyncOutput::default();

    let config = parse_toolchain_config::<ComposerToolchainConfig>(input.toolchain_config.clone())?;
    let settings = config.to_sync_settings();
    let manifest_path = input
        .context
        .get_project_root(&input.project)
        .join(MANIFEST);

    if !is_project_toolchain_enabled(&input.project)
        || !(settings.sync_requirements || settings.sync_repositories)
        || !manifest_path.exists()
    {
        output.skipped = true;

        return Ok(Json(output));
    }

    let (op, changed) = Operation::track("sync-project-workspace-dependencies", || {
        sync_project_manifest(&input, &settings, &manifest_path)
    })?;

    output.operations.push(op);

    if changed {
        output.changed_files.push(manifest_path);
    }

    Ok(Json(output))
}

fn sync_project_manifest(
    input: &SyncProjectInput,
    settings: &SyncSettings,
    manifest_path: &VirtualPath,
) -> AnyResult<bool> {
    let content = fs::read_to_string(manifest_path)?;

    let Value::Object(mut root) = serde_json::from_str::<Value>(&content)
        .map_err(|error| anyhow!("Failed to parse <path>{manifest_path}</path>: {error}"))?
    else {
        return Err(anyhow!(
            "Expected <path>{manifest_path}</path> to contain an object."
        ));
    };

    let packages = collect_workspace_packages(input, settings.sync_repositories)?;

    let report = sync_manifest(&mut root, &packages, settings, &|url: &str| {
        resolve_workspace_package(input, url)
    });

    if report.changes.is_empty() {
        return Ok(false);
    }

    for change in &report.changes {
        host_log!(
            debug,
            "{change} in <id>{}</id>'s <file>{MANIFEST}</file>",
            input.project.id
        );
    }

    fs::write(manifest_path, format_manifest(&root, &content))?;

    // Worth a warning rather than a debug line: the plugin can't run
    // Composer, so refreshing the lock file is left to whoever reads this.
    // The packages are named because nothing shorter works: `--lock` alone
    // fails on a package that was removed (it stays pinned to its locked
    // version, which the repository no longer provides), and a bare
    // `composer update` would upgrade every dependency of the project.
    let lock_notice = if manifest_path.with_file_name(LOCK).exists() {
        let arguments = if report.packages.is_empty() {
            "--lock".to_owned()
        } else {
            report.packages.join(" ")
        };

        format!(" Update its <file>{LOCK}</file> with <shell>composer update {arguments}</shell>.")
    } else {
        String::new()
    };

    host_log!(
        warn,
        "Synced <file>{MANIFEST}</file> of <id>{}</id> with the projects it depends on: {}.{lock_notice}",
        input.project.id,
        report.changes.join(", ")
    );

    Ok(true)
}

/// The workspace packages the project depends on: the ones of its own
/// `dependsOn`, then, when the repositories are synced, the ones those
/// packages depend on themselves.
fn collect_workspace_packages(
    input: &SyncProjectInput,
    with_transitive: bool,
) -> AnyResult<Vec<WorkspacePackage>> {
    let mut packages: Vec<WorkspacePackage> = vec![];
    let mut visited = HashSet::from([input.project.id.to_string()]);
    let mut queue = VecDeque::new();

    for dep in &input.project_dependencies {
        let section = match dep.dependency_scope {
            Some(DependencyScope::Production | DependencyScope::Peer) => Section::Require,
            Some(DependencyScope::Development) => Section::RequireDev,
            // Not package relationships: `build` is the scope moon gives to
            // the projects a task depends on, and `root` is the root project.
            // The JavaScript toolchain leaves them out as well.
            Some(DependencyScope::Build | DependencyScope::Root) | None => continue,
        };

        if !visited.insert(dep.id.to_string()) || !is_project_toolchain_enabled(dep) {
            continue;
        }

        if let Some(package) = read_workspace_package(input, &dep.source, Some(section))? {
            packages.push(package);
            queue.push_back(dep.id.to_string());
        }
    }

    if !with_transitive {
        return Ok(packages);
    }

    while let Some(id) = queue.pop_front() {
        let project = match load_graph_project(&id) {
            Ok(project) => project,
            Err(error) => {
                host_log!(
                    warn,
                    "Failed to load the dependencies of <id>{id}</id>, they won't be added to the repositories of <id>{}</id>: {error}",
                    input.project.id
                );

                continue;
            }
        };

        for dep in &project.dependencies {
            if !dep.is_installed_with_dependent() || !visited.insert(dep.id.clone()) {
                continue;
            }

            let Ok(dep_project) = load_graph_project(&dep.id) else {
                continue;
            };

            if let Some(package) = read_workspace_package(input, &dep_project.source, None)? {
                packages.push(package);
                queue.push_back(dep.id.clone());
            }
        }
    }

    Ok(packages)
}

/// Reads the package of the project at `source` (relative to the workspace
/// root), if it has a `composer.json` that names it.
fn read_workspace_package(
    input: &SyncProjectInput,
    source: &str,
    section: Option<Section>,
) -> AnyResult<Option<WorkspacePackage>> {
    let manifest_path = input
        .context
        .get_project_root_from_source(source)
        .join(MANIFEST);

    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .map_err(|error| anyhow!("Failed to parse <path>{manifest_path}</path>: {error}"))?;

    let field = |name: &str| {
        manifest
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };

    Ok(field("name").map(|name| WorkspacePackage {
        name,
        version: field("version"),
        path: relative_path(&input.project.source, source),
        section,
    }))
}

/// Name of the workspace package a `path` repository of the project points
/// to, when its URL leads to a moon project.
fn resolve_workspace_package(input: &SyncProjectInput, url: &str) -> Option<String> {
    let source = resolve_workspace_path(&input.project.source, url)?;
    let root = input.context.get_project_root_from_source(&source);

    if !PROJECT_CONFIGS.iter().any(|name| root.join(name).exists()) {
        return None;
    }

    read_workspace_package(input, &source, None)
        .ok()
        .flatten()
        .map(|package| package.name)
}
