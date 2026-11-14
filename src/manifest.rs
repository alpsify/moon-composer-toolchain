//! `composer.json` manipulation, kept free of any WASM or host concern so
//! that it can be unit tested natively (`cargo test --no-default-features`).

use serde::Serialize;
use serde_json::ser::{PrettyFormatter, Serializer};
use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;

/// The section of `composer.json` a workspace package is required from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Require,
    RequireDev,
}

impl Section {
    pub fn key(self) -> &'static str {
        match self {
            Section::Require => "require",
            Section::RequireDev => "require-dev",
        }
    }

    fn other(self) -> Section {
        match self {
            Section::Require => Section::RequireDev,
            Section::RequireDev => Section::Require,
        }
    }
}

/// A Composer package that lives in the workspace and that the project being
/// synced depends on.
#[derive(Clone, Debug)]
pub struct WorkspacePackage {
    /// `name` of the package's `composer.json`.
    pub name: String,

    /// `version` of the package's `composer.json`, when it declares one.
    pub version: Option<String>,

    /// Path of the package, relative to the project being synced.
    pub path: String,

    /// Where the project requires it from. `None` for a package that is only
    /// reached through another workspace package: Composer reads
    /// `repositories` from the root package alone, so it still has to be
    /// declared there, but it is not a requirement of the project itself.
    pub section: Option<Section>,
}

#[derive(Clone, Debug)]
pub struct SyncSettings {
    /// Sync `require` and `require-dev`.
    pub sync_requirements: bool,

    /// Constraint of a requirement added by the sync.
    pub constraint: String,

    /// Remove the workspace packages the project no longer depends on.
    pub prune: bool,

    /// Sync `repositories`.
    pub sync_repositories: bool,

    /// URL of the one `path` repository shared by every workspace package
    /// (`../packages/*`). Without it, each package gets its own repository,
    /// pointing to where it lives in the workspace.
    pub repository_url: Option<String>,

    /// Value of the repositories' `options.symlink`, left alone when `None`.
    pub repository_symlink: Option<bool>,

    /// Version given in the repositories' `options.versions` to a package that
    /// declares none.
    pub repository_version: Option<String>,
}

/// What a sync changed in a manifest.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Description of every change made, none when it was already in sync.
    pub changes: Vec<String>,

    /// Names of the workspace packages those changes are about, which are the
    /// ones to hand to `composer update` to bring the lock file up to date.
    pub packages: Vec<String>,
}

impl SyncReport {
    /// Records a change to the requirement or to the repository of a package.
    fn package(&mut self, name: &str, change: String) {
        if !self.packages.iter().any(|package| package == name) {
            self.packages.push(name.to_owned());
        }

        self.changes.push(change);
    }

    /// Records a change that is about no package in particular.
    fn other(&mut self, change: String) {
        self.changes.push(change);
    }
}

/// Syncs `require`, `require-dev` and `repositories` of a `composer.json`
/// with the workspace packages the project depends on, and reports what it
/// changed.
///
/// `resolve_path_repository` maps the URL of an existing `path` repository to
/// the name of the workspace package it points to, if it is one. It is only
/// called when each package has its own repository.
pub fn sync_manifest(
    root: &mut JsonMap,
    packages: &[WorkspacePackage],
    settings: &SyncSettings,
    resolve_path_repository: &dyn Fn(&str) -> Option<String>,
) -> SyncReport {
    let mut report = SyncReport::default();

    // Has to be read before the repositories are synced, as they are the
    // record of what a previous sync added.
    let known = known_workspace_packages(root, settings, resolve_path_repository);

    if settings.sync_requirements {
        sync_requirements(root, packages, &known, settings, &mut report);
    }

    if settings.sync_repositories {
        match &settings.repository_url {
            Some(url) => sync_shared_repository(root, packages, url, settings, &mut report),
            None => sync_package_repositories(
                root,
                packages,
                settings,
                resolve_path_repository,
                &mut report,
            ),
        }
    }

    report
}

/// Names of the workspace packages the manifest already knows about, which is
/// what tells a requirement the sync may remove (`lychen/core`) from one it
/// must never touch (`symfony/console`).
fn known_workspace_packages(
    root: &JsonMap,
    settings: &SyncSettings,
    resolve_path_repository: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut names = vec![];

    for repo in repositories(root) {
        let Some(url) = path_repository_url(repo) else {
            continue;
        };

        match &settings.repository_url {
            Some(shared_url) => {
                if same_url(url, shared_url)
                    && let Some(versions) = repository_versions(repo.get("options"))
                {
                    names.extend(versions.keys().cloned());
                }
            }
            None => names.extend(resolve_path_repository(url)),
        }
    }

    names
}

// REQUIREMENTS

fn sync_requirements(
    root: &mut JsonMap,
    packages: &[WorkspacePackage],
    known: &[String],
    settings: &SyncSettings,
    report: &mut SyncReport,
) {
    let sorted = root
        .get("config")
        .and_then(|config| config.get("sort-packages"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    for package in packages {
        let Some(section) = package.section else {
            continue;
        };

        if !ensure_section(root, section) {
            continue;
        }

        // A requirement that changed scope keeps its constraint
        let moved = remove_requirement(root, section.other(), &package.name);

        let Some(requirements) = root.get_mut(section.key()).and_then(Value::as_object_mut) else {
            continue;
        };

        if requirements.contains_key(&package.name) {
            if moved.is_some() {
                report.package(
                    &package.name,
                    format!(
                        "Removed {} from {}, already in {}",
                        package.name,
                        section.other().key(),
                        section.key()
                    ),
                );
            }

            continue;
        }

        report.package(
            &package.name,
            match &moved {
                Some(_) => format!(
                    "Moved {} from {} to {}",
                    package.name,
                    section.other().key(),
                    section.key()
                ),
                None => format!("Added {} to {}", package.name, section.key()),
            },
        );

        insert_requirement(
            requirements,
            &package.name,
            moved.unwrap_or_else(|| Value::String(settings.constraint.clone())),
            sorted,
        );
    }

    if settings.prune {
        for name in known {
            if packages
                .iter()
                .any(|package| package.section.is_some() && &package.name == name)
            {
                continue;
            }

            for section in [Section::Require, Section::RequireDev] {
                if remove_requirement(root, section, name).is_some() {
                    report.package(name, format!("Removed {name} from {}", section.key()));
                }
            }
        }
    }
}

/// Creates the section when the manifest has none, and tells whether it can
/// take requirements.
fn ensure_section(root: &mut JsonMap, section: Section) -> bool {
    let key = section.key();

    // PHP encodes an empty map as an empty list
    let is_empty_list = root
        .get(key)
        .and_then(Value::as_array)
        .is_some_and(|list| list.is_empty());

    if is_empty_list {
        root.insert(key.into(), Value::Object(JsonMap::new()));
    } else if !root.contains_key(key) {
        insert_after(root, key, Value::Object(JsonMap::new()), &["require"]);
    }

    root.get(key).is_some_and(Value::is_object)
}

fn remove_requirement(root: &mut JsonMap, section: Section, name: &str) -> Option<Value> {
    root.get_mut(section.key())
        .and_then(Value::as_object_mut)
        .and_then(|requirements| remove_ordered(requirements, name))
}

fn insert_requirement(requirements: &mut JsonMap, name: &str, constraint: Value, sorted: bool) {
    let index = if sorted {
        let key = sort_key(name);

        requirements
            .keys()
            .position(|existing| sort_key(existing) > key)
            .unwrap_or(requirements.len())
    } else {
        requirements.len()
    };

    insert_at(requirements, index, name, constraint);
}

/// The order Composer's `sort-packages` gives to requirements: the platform
/// packages first (`php`, `ext-*`, `lib-*`), then the others alphabetically.
fn sort_key(name: &str) -> (u8, String) {
    let name = name.to_lowercase();

    let rank = if name.contains('/') {
        5
    } else if name == "php" || name.starts_with("php-") {
        0
    } else if name.starts_with("hhvm") {
        1
    } else if name.starts_with("ext-") {
        2
    } else if name.starts_with("lib-") {
        3
    } else {
        4
    };

    (rank, name)
}

// REPOSITORIES

/// One `path` repository for every workspace package, which get their version
/// from its `options.versions`.
fn sync_shared_repository(
    root: &mut JsonMap,
    packages: &[WorkspacePackage],
    url: &str,
    settings: &SyncSettings,
    report: &mut SyncReport,
) {
    let is_shared = |repo: &Value| path_repository_url(repo).is_some_and(|u| same_url(u, url));

    if packages.is_empty() {
        if settings.prune && remove_repositories(root, &is_shared) {
            report.other(format!("Removed the path repository {url}"));
        }

        return;
    }

    let created = !repositories(root).any(&is_shared);

    if created {
        let mut repo = JsonMap::new();
        repo.insert("type".into(), Value::String("path".into()));
        repo.insert("url".into(), Value::String(url.into()));

        add_repository(root, "workspace", Value::Object(repo));
        report.other(format!("Added the path repository {url}"));
    }

    let Some(repo) = repositories_mut(root)
        .into_iter()
        .find(|repo| is_shared(repo))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    // Adding the repository says it all, its options are not changes of their own
    if created {
        sync_repository_symlink(repo, url, settings, &mut SyncReport::default());
    } else {
        sync_repository_symlink(repo, url, settings, report);
    }

    let current = repository_versions(repo.get("options"))
        .cloned()
        .unwrap_or_default();
    let mut versions = JsonMap::new();

    // Existing entries first, to leave their order alone
    for (name, version) in &current {
        match packages.iter().find(|package| &package.name == name) {
            Some(package) => {
                let version = match package_version(package, settings) {
                    Some(expected) => Value::String(expected),
                    None => version.clone(),
                };

                versions.insert(name.clone(), version);
            }
            None if settings.prune => {
                report.package(
                    name,
                    format!("Removed {name} from the path repository {url}"),
                );
            }
            None => {
                versions.insert(name.clone(), version.clone());
            }
        }
    }

    for package in packages {
        if !current.contains_key(&package.name)
            && let Some(version) = package_version(package, settings)
        {
            versions.insert(package.name.clone(), Value::String(version));
            report.package(
                &package.name,
                format!("Added {} to the path repository {url}", package.name),
            );
        }
    }

    for (name, version) in &versions {
        if current
            .get(name)
            .is_some_and(|previous| previous != version)
        {
            report.package(
                name,
                format!("Updated the version of {name} in the path repository {url}"),
            );
        }
    }

    if versions != current {
        set_repository_option(repo, "versions", Value::Object(versions));
    }
}

/// A `path` repository per workspace package, pointing to where it lives.
fn sync_package_repositories(
    root: &mut JsonMap,
    packages: &[WorkspacePackage],
    settings: &SyncSettings,
    resolve_path_repository: &dyn Fn(&str) -> Option<String>,
    report: &mut SyncReport,
) {
    if settings.prune {
        let is_stale = |repo: &Value| {
            path_repository_url(repo)
                .and_then(resolve_path_repository)
                .is_some_and(|name| !packages.iter().any(|package| package.name == name))
        };

        let stale = repositories(root)
            .filter(|repo| is_stale(repo))
            .filter_map(path_repository_url)
            .filter_map(|url| resolve_path_repository(url).map(|name| (url.to_owned(), name)))
            .collect::<Vec<_>>();

        if remove_repositories(root, &is_stale) {
            for (url, name) in stale {
                report.package(&name, format!("Removed the path repository {url}"));
            }
        }
    }

    for package in packages {
        let points_to_package = |repo: &Value| {
            path_repository_url(repo).is_some_and(|url| {
                same_url(url, &package.path)
                    || resolve_path_repository(url).is_some_and(|name| name == package.name)
            })
        };

        let created = !repositories(root).any(&points_to_package);

        if created {
            let mut repo = JsonMap::new();
            repo.insert("type".into(), Value::String("path".into()));
            repo.insert("url".into(), Value::String(package.path.clone()));

            add_repository(root, &package.name, Value::Object(repo));
            report.package(
                &package.name,
                format!("Added the path repository {}", package.path),
            );
        }

        let Some(repo) = repositories_mut(root)
            .into_iter()
            .find(|repo| points_to_package(repo))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };

        // Adding the repository says it all, its options are not changes of
        // their own
        let mut unreported = SyncReport::default();
        let report = if created {
            &mut unreported
        } else {
            &mut *report
        };

        sync_repository_symlink(repo, &package.path, settings, report);

        if let Some(version) = package_version(package, settings) {
            let version = Value::String(version);
            let mut versions = repository_versions(repo.get("options"))
                .cloned()
                .unwrap_or_default();

            if versions.get(&package.name) != Some(&version) {
                versions.insert(package.name.clone(), version);
                set_repository_option(repo, "versions", Value::Object(versions));
                report.package(
                    &package.name,
                    format!(
                        "Set the version of {} in the path repository {}",
                        package.name, package.path
                    ),
                );
            }
        }
    }
}

fn package_version(package: &WorkspacePackage, settings: &SyncSettings) -> Option<String> {
    package
        .version
        .clone()
        .or_else(|| settings.repository_version.clone())
}

fn sync_repository_symlink(
    repo: &mut JsonMap,
    url: &str,
    settings: &SyncSettings,
    report: &mut SyncReport,
) {
    let Some(symlink) = settings.repository_symlink else {
        return;
    };

    let current = repo
        .get("options")
        .and_then(|options| options.get("symlink"))
        .and_then(Value::as_bool);

    if current != Some(symlink) {
        set_repository_option(repo, "symlink", Value::Bool(symlink));
        report.other(format!(
            "Set symlink to {symlink} in the path repository {url}"
        ));
    }
}

fn set_repository_option(repo: &mut JsonMap, name: &str, value: Value) {
    if !repo.get("options").is_some_and(Value::is_object) {
        repo.insert("options".into(), Value::Object(JsonMap::new()));
    }

    if let Some(options) = repo.get_mut("options").and_then(Value::as_object_mut) {
        options.insert(name.into(), value);
    }
}

/// `options.versions` of a repository, given its `options`.
fn repository_versions(options: Option<&Value>) -> Option<&JsonMap> {
    options
        .and_then(|options| options.get("versions"))
        .and_then(Value::as_object)
}

/// URL of the repository when it is a `path` one.
fn path_repository_url(repo: &Value) -> Option<&str> {
    if repo.get("type").and_then(Value::as_str) != Some("path") {
        return None;
    }

    repo.get("url").and_then(Value::as_str)
}

/// `repositories` is a list, or a map of named repositories.
fn repositories(root: &JsonMap) -> Box<dyn Iterator<Item = &Value> + '_> {
    match root.get("repositories") {
        Some(Value::Array(list)) => Box::new(list.iter()),
        Some(Value::Object(map)) => Box::new(map.values()),
        _ => Box::new(std::iter::empty()),
    }
}

fn repositories_mut(root: &mut JsonMap) -> Vec<&mut Value> {
    match root.get_mut("repositories") {
        Some(Value::Array(list)) => list.iter_mut().collect(),
        Some(Value::Object(map)) => map.values_mut().collect(),
        _ => vec![],
    }
}

fn add_repository(root: &mut JsonMap, name: &str, repo: Value) {
    match root.get_mut("repositories") {
        Some(Value::Array(list)) => list.push(repo),
        Some(Value::Object(map)) => {
            map.insert(name.into(), repo);
        }
        _ => {
            root.remove("repositories");

            insert_after(
                root,
                "repositories",
                Value::Array(vec![repo]),
                &["require-dev", "require"],
            );
        }
    }
}

/// Removes the matching repositories, and `repositories` itself once empty.
fn remove_repositories(root: &mut JsonMap, matches: &dyn Fn(&Value) -> bool) -> bool {
    let (removed, empty) = match root.get_mut("repositories") {
        Some(Value::Array(list)) => {
            let before = list.len();
            list.retain(|repo| !matches(repo));

            (list.len() != before, list.is_empty())
        }
        Some(Value::Object(map)) => {
            let before = map.len();

            for (name, repo) in std::mem::take(map) {
                if !matches(&repo) {
                    map.insert(name, repo);
                }
            }

            (map.len() != before, map.is_empty())
        }
        _ => (false, false),
    };

    if removed && empty {
        remove_ordered(root, "repositories");
    }

    removed
}

fn same_url(left: &str, right: &str) -> bool {
    fn normalize(url: &str) -> &str {
        let url = url.strip_prefix("./").unwrap_or(url);

        url.strip_suffix('/').unwrap_or(url)
    }

    normalize(left) == normalize(right)
}

// ORDERED MAPS

/// Removes a key and leaves every other key where it was, which
/// `Map::remove` does not: it moves the last entry into the freed slot.
fn remove_ordered(map: &mut JsonMap, key: &str) -> Option<Value> {
    if !map.contains_key(key) {
        return None;
    }

    let mut removed = None;

    for (name, value) in std::mem::take(map) {
        if name == key {
            removed = Some(value);
        } else {
            map.insert(name, value);
        }
    }

    removed
}

fn insert_at(map: &mut JsonMap, index: usize, key: &str, value: Value) {
    let mut pending = Some(value);

    for (position, (name, existing)) in std::mem::take(map).into_iter().enumerate() {
        if position == index
            && let Some(value) = pending.take()
        {
            map.insert(key.into(), value);
        }

        map.insert(name, existing);
    }

    if let Some(value) = pending {
        map.insert(key.into(), value);
    }
}

/// Inserts after the first of the anchors the map has, or at the end.
fn insert_after(map: &mut JsonMap, key: &str, value: Value, anchors: &[&str]) {
    let index = anchors
        .iter()
        .find_map(|anchor| map.keys().position(|name| name == anchor))
        .map_or(map.len(), |position| position + 1);

    insert_at(map, index, key, value);
}

// PATHS

/// Path of `to` relative to `from`, both relative to the workspace root.
pub fn relative_path(from: &str, to: &str) -> String {
    let from = segments(from);
    let to = segments(to);

    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();

    let mut parts = vec![".."; from.len() - common];
    parts.extend(&to[common..]);

    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// Resolves the URL of a `path` repository of the project at `from` to a
/// path relative to the workspace root. `None` when it is not a plain
/// relative path (absolute, home, glob) or when it leaves the workspace.
pub fn resolve_workspace_path(from: &str, url: &str) -> Option<String> {
    if url.is_empty()
        || url.starts_with(['/', '\\', '~'])
        || url.contains([':', '*', '?', '[', '{'])
    {
        return None;
    }

    let mut resolved = segments(from);

    for segment in segments(url) {
        if segment == ".." {
            resolved.pop()?;
        } else {
            resolved.push(segment);
        }
    }

    Some(resolved.join("/"))
}

fn segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

// FORMATTING

/// Indentation of a JSON document: the whitespace before its first indented
/// line, or Composer's own 4 spaces.
pub fn detect_indent(content: &str) -> &str {
    content
        .lines()
        .find_map(|line| {
            let rest = line.trim_start_matches([' ', '\t']);

            (!rest.is_empty() && rest.len() < line.len()).then(|| &line[..line.len() - rest.len()])
        })
        .unwrap_or("    ")
}

/// Serializes a manifest the way `original` was written: same indentation,
/// line endings and trailing newline.
pub fn format_manifest(root: &JsonMap, original: &str) -> String {
    let mut buffer = vec![];
    let formatter = PrettyFormatter::with_indent(detect_indent(original).as_bytes());

    root.serialize(&mut Serializer::with_formatter(&mut buffer, formatter))
        .expect("a JSON map is always serializable");

    let mut content = String::from_utf8(buffer).expect("serde_json only writes UTF-8");

    if original.ends_with('\n') {
        content.push('\n');
    }

    if original.contains("\r\n") {
        content = content.replace('\n', "\r\n");
    }

    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings() -> SyncSettings {
        SyncSettings {
            sync_requirements: true,
            constraint: "*".into(),
            prune: true,
            sync_repositories: true,
            repository_url: Some("../packages/*".into()),
            repository_symlink: Some(false),
            repository_version: Some("0.0.0".into()),
        }
    }

    fn package(name: &str, section: Option<Section>) -> WorkspacePackage {
        let dir = name.rsplit('/').next().unwrap();

        WorkspacePackage {
            name: name.into(),
            version: None,
            path: format!("../../../libs/php/{dir}"),
            section,
        }
    }

    fn prod(name: &str) -> WorkspacePackage {
        package(name, Some(Section::Require))
    }

    fn dev(name: &str) -> WorkspacePackage {
        package(name, Some(Section::RequireDev))
    }

    fn manifest(value: Value) -> JsonMap {
        value.as_object().unwrap().clone()
    }

    fn no_resolve(_: &str) -> Option<String> {
        None
    }

    /// The description of the changes, which is all most tests look at.
    fn sync_manifest(
        root: &mut JsonMap,
        packages: &[WorkspacePackage],
        settings: &SyncSettings,
        resolve_path_repository: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<String> {
        super::sync_manifest(root, packages, settings, resolve_path_repository).changes
    }

    /// Resolves `../../../libs/php/<dir>` to `lychen/<dir>`.
    fn resolve_libs(url: &str) -> Option<String> {
        url.strip_prefix("../../../libs/php/")
            .map(|dir| format!("lychen/{dir}"))
    }

    fn keys(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// What the Lychen APIs look like.
    fn api_manifest() -> JsonMap {
        manifest(json!({
            "name": "lychen/tera-api",
            "require": {
                "php": ">=8.4",
                "ext-ctype": "*",
                "api-platform/core": "5.0.*",
                "lychen/core": "*",
                "lychen/model": "*",
                "symfony/yaml": "8.1.*"
            },
            "require-dev": {
                "lychen/test": "*",
                "phpunit/phpunit": "13.4.*"
            },
            "repositories": [
                {
                    "type": "path",
                    "url": "../packages/*",
                    "options": {
                        "symlink": false,
                        "versions": {
                            "lychen/model": "0.0.0",
                            "lychen/test": "0.0.0",
                            "lychen/core": "0.0.0"
                        }
                    }
                }
            ],
            "config": {
                "sort-packages": true
            }
        }))
    }

    fn api_packages() -> Vec<WorkspacePackage> {
        vec![
            prod("lychen/core"),
            prod("lychen/model"),
            dev("lychen/test"),
        ]
    }

    mod requirements {
        use super::*;

        #[test]
        fn leaves_a_synced_manifest_alone() {
            let mut root = api_manifest();
            let changes = sync_manifest(&mut root, &api_packages(), &settings(), &no_resolve);

            let expected = api_manifest();

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(root, expected);
            assert_eq!(keys(&root["require"]), keys(&expected["require"]));
        }

        #[test]
        fn reports_the_packages_to_update_in_the_lock_file() {
            let mut root = api_manifest();
            let packages = vec![prod("lychen/core"), dev("lychen/model"), prod("lychen/api")];

            // Moved, added and removed, in that order. Nothing happens to core.
            let report =
                crate::manifest::sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(
                report.packages,
                ["lychen/model", "lychen/api", "lychen/test"]
            );

            let report =
                crate::manifest::sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(report.packages, Vec::<String>::new());
        }

        #[test]
        fn adds_each_dependency_to_the_section_of_its_scope() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let changes = sync_manifest(&mut root, &api_packages(), &settings(), &no_resolve);

            assert_eq!(
                root["require"],
                json!({ "lychen/core": "*", "lychen/model": "*" })
            );
            assert_eq!(root["require-dev"], json!({ "lychen/test": "*" }));
            assert_eq!(
                root.keys().collect::<Vec<_>>(),
                ["name", "require", "require-dev", "repositories"]
            );
            assert!(changes.contains(&"Added lychen/core to require".to_owned()));
            assert!(changes.contains(&"Added lychen/test to require-dev".to_owned()));
        }

        #[test]
        fn uses_the_configured_constraint() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let settings = SyncSettings {
                constraint: "@dev".into(),
                ..settings()
            };

            sync_manifest(&mut root, &[prod("lychen/core")], &settings, &no_resolve);

            assert_eq!(root["require"], json!({ "lychen/core": "@dev" }));
        }

        #[test]
        fn keeps_an_existing_constraint() {
            let mut root = api_manifest();
            root["require"]["lychen/core"] = json!("^1.2");

            let changes = sync_manifest(&mut root, &api_packages(), &settings(), &no_resolve);

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(root["require"]["lychen/core"], "^1.2");
        }

        #[test]
        fn inserts_where_sort_packages_would() {
            let mut root = api_manifest();
            let mut packages = api_packages();
            packages.push(prod("lychen/api"));
            packages.push(prod("lychen/zitadel"));

            sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(
                keys(&root["require"]),
                [
                    "php",
                    "ext-ctype",
                    "api-platform/core",
                    "lychen/api",
                    "lychen/core",
                    "lychen/model",
                    "lychen/zitadel",
                    "symfony/yaml"
                ]
            );
        }

        #[test]
        fn appends_without_sort_packages() {
            let mut root = api_manifest();
            root.remove("config");

            let mut packages = api_packages();
            packages.push(prod("lychen/api"));

            sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(keys(&root["require"]).last(), Some(&"lychen/api"));
        }

        #[test]
        fn moves_a_dependency_whose_scope_changed() {
            let mut root = api_manifest();
            root["require"]["lychen/core"] = json!("^1.2");

            let packages = vec![
                dev("lychen/core"),
                prod("lychen/model"),
                prod("lychen/test"),
            ];
            let changes = sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(
                keys(&root["require"]),
                [
                    "php",
                    "ext-ctype",
                    "api-platform/core",
                    "lychen/model",
                    "lychen/test",
                    "symfony/yaml"
                ]
            );
            assert_eq!(
                root["require-dev"],
                json!({ "lychen/core": "^1.2", "phpunit/phpunit": "13.4.*" })
            );
            assert_eq!(
                changes,
                [
                    "Moved lychen/core from require to require-dev",
                    "Moved lychen/test from require-dev to require"
                ]
            );
        }

        #[test]
        fn removes_a_dependency_the_project_dropped() {
            let mut root = api_manifest();
            let packages = vec![prod("lychen/core")];
            let changes = sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(
                keys(&root["require"]),
                [
                    "php",
                    "ext-ctype",
                    "api-platform/core",
                    "lychen/core",
                    "symfony/yaml"
                ]
            );
            assert_eq!(root["require-dev"], json!({ "phpunit/phpunit": "13.4.*" }));
            assert_eq!(
                root["repositories"][0]["options"]["versions"],
                json!({ "lychen/core": "0.0.0" })
            );
            assert!(changes.contains(&"Removed lychen/model from require".to_owned()));
            assert!(changes.contains(&"Removed lychen/test from require-dev".to_owned()));
        }

        #[test]
        fn keeps_a_dropped_dependency_without_prune() {
            let mut root = api_manifest();
            let settings = SyncSettings {
                prune: false,
                ..settings()
            };
            let changes = sync_manifest(&mut root, &[prod("lychen/core")], &settings, &no_resolve);

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(root, api_manifest());
        }

        #[test]
        fn never_removes_a_package_it_does_not_know() {
            let mut root = api_manifest();

            sync_manifest(&mut root, &[], &settings(), &no_resolve);

            assert_eq!(
                root["require"],
                json!({
                    "php": ">=8.4",
                    "ext-ctype": "*",
                    "api-platform/core": "5.0.*",
                    "symfony/yaml": "8.1.*"
                })
            );
            assert_eq!(root["require-dev"], json!({ "phpunit/phpunit": "13.4.*" }));
        }

        #[test]
        fn does_not_require_a_transitive_package() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let packages = vec![prod("lychen/test"), package("lychen/core", None)];

            sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(root["require"], json!({ "lychen/test": "*" }));
            assert_eq!(
                root["repositories"][0]["options"]["versions"],
                json!({ "lychen/test": "0.0.0", "lychen/core": "0.0.0" })
            );
        }

        #[test]
        fn can_be_disabled() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let settings = SyncSettings {
                sync_requirements: false,
                ..settings()
            };

            sync_manifest(&mut root, &api_packages(), &settings, &no_resolve);

            assert!(!root.contains_key("require"));
            assert!(root.contains_key("repositories"));
        }
    }

    mod shared_repository {
        use super::*;

        #[test]
        fn creates_the_repository() {
            let mut root = manifest(json!({
                "name": "lychen/tera-api",
                "require": {},
                "config": {}
            }));

            sync_manifest(&mut root, &api_packages(), &settings(), &no_resolve);

            assert_eq!(
                root["repositories"],
                json!([
                    {
                        "type": "path",
                        "url": "../packages/*",
                        "options": {
                            "symlink": false,
                            "versions": {
                                "lychen/core": "0.0.0",
                                "lychen/model": "0.0.0",
                                "lychen/test": "0.0.0"
                            }
                        }
                    }
                ])
            );
            assert_eq!(
                root.keys().collect::<Vec<_>>(),
                ["name", "require", "require-dev", "repositories", "config"]
            );
        }

        #[test]
        fn keeps_the_other_repositories() {
            let mut root = api_manifest();
            root["repositories"].as_array_mut().unwrap().insert(
                0,
                json!({ "type": "vcs", "url": "https://example.com/fork.git" }),
            );

            sync_manifest(&mut root, &[], &settings(), &no_resolve);

            assert_eq!(
                root["repositories"],
                json!([{ "type": "vcs", "url": "https://example.com/fork.git" }])
            );
        }

        #[test]
        fn removes_the_repository_with_its_last_package() {
            let mut root = api_manifest();
            let changes = sync_manifest(&mut root, &[], &settings(), &no_resolve);

            assert!(!root.contains_key("repositories"));
            assert!(changes.contains(&"Removed the path repository ../packages/*".to_owned()));
        }

        #[test]
        fn adds_nothing_to_a_project_without_dependencies() {
            let mut root = manifest(json!({ "name": "lychen/core", "require": { "php": "^8.4" } }));
            let changes = sync_manifest(&mut root, &[], &settings(), &no_resolve);

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(
                root,
                manifest(json!({ "name": "lychen/core", "require": { "php": "^8.4" } }))
            );
        }

        #[test]
        fn uses_the_version_a_package_declares() {
            let mut root = api_manifest();
            let mut packages = api_packages();
            packages[0].version = Some("1.4.0".into());

            let changes = sync_manifest(&mut root, &packages, &settings(), &no_resolve);

            assert_eq!(
                root["repositories"][0]["options"]["versions"],
                json!({
                    "lychen/model": "0.0.0",
                    "lychen/test": "0.0.0",
                    "lychen/core": "1.4.0"
                })
            );
            assert_eq!(
                changes,
                ["Updated the version of lychen/core in the path repository ../packages/*"]
            );
        }

        #[test]
        fn leaves_symlink_alone_when_not_configured() {
            let mut root = api_manifest();
            root["repositories"][0]["options"]["symlink"] = json!(true);

            let settings = SyncSettings {
                repository_symlink: None,
                ..settings()
            };
            let changes = sync_manifest(&mut root, &api_packages(), &settings, &no_resolve);

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(root["repositories"][0]["options"]["symlink"], true);
        }

        #[test]
        fn supports_named_repositories() {
            let mut root = manifest(json!({
                "name": "lychen/tera-api",
                "repositories": {
                    "fork": { "type": "vcs", "url": "https://example.com/fork.git" }
                }
            }));

            sync_manifest(&mut root, &[prod("lychen/core")], &settings(), &no_resolve);

            assert_eq!(keys(&root["repositories"]), ["fork", "workspace"]);
            assert_eq!(
                root["repositories"]["workspace"]["options"]["versions"],
                json!({ "lychen/core": "0.0.0" })
            );

            sync_manifest(&mut root, &[], &settings(), &no_resolve);

            assert_eq!(keys(&root["repositories"]), ["fork"]);
        }

        #[test]
        fn can_be_disabled() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let settings = SyncSettings {
                sync_repositories: false,
                ..settings()
            };

            sync_manifest(&mut root, &api_packages(), &settings, &no_resolve);

            assert!(root.contains_key("require"));
            assert!(!root.contains_key("repositories"));
        }
    }

    mod package_repositories {
        use super::*;

        fn settings() -> SyncSettings {
            SyncSettings {
                repository_url: None,
                repository_symlink: None,
                ..super::settings()
            }
        }

        #[test]
        fn adds_a_repository_per_package() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let packages = vec![prod("lychen/core"), package("lychen/model", None)];

            sync_manifest(&mut root, &packages, &settings(), &resolve_libs);

            assert_eq!(
                root["repositories"],
                json!([
                    {
                        "type": "path",
                        "url": "../../../libs/php/core",
                        "options": { "versions": { "lychen/core": "0.0.0" } }
                    },
                    {
                        "type": "path",
                        "url": "../../../libs/php/model",
                        "options": { "versions": { "lychen/model": "0.0.0" } }
                    }
                ])
            );
        }

        #[test]
        fn reports_a_new_repository_as_a_single_change() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let settings = SyncSettings {
                repository_symlink: Some(true),
                ..settings()
            };

            let changes =
                sync_manifest(&mut root, &[prod("lychen/core")], &settings, &resolve_libs);

            assert_eq!(
                changes,
                [
                    "Added lychen/core to require",
                    "Added the path repository ../../../libs/php/core"
                ]
            );
            assert_eq!(
                root["repositories"][0]["options"],
                json!({ "symlink": true, "versions": { "lychen/core": "0.0.0" } })
            );

            // Unlike a change to a repository that is already there
            let settings = SyncSettings {
                repository_symlink: Some(false),
                ..settings
            };

            let changes =
                sync_manifest(&mut root, &[prod("lychen/core")], &settings, &resolve_libs);

            assert_eq!(
                changes,
                ["Set symlink to false in the path repository ../../../libs/php/core"]
            );
        }

        #[test]
        fn is_stable_once_synced() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));

            sync_manifest(&mut root, &api_packages(), &settings(), &resolve_libs);

            let synced = root.clone();
            let changes = sync_manifest(&mut root, &api_packages(), &settings(), &resolve_libs);

            assert_eq!(changes, Vec::<String>::new());
            assert_eq!(root, synced);
        }

        #[test]
        fn writes_no_options_without_a_version_or_symlink() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let settings = SyncSettings {
                repository_version: None,
                ..settings()
            };

            sync_manifest(&mut root, &[prod("lychen/core")], &settings, &resolve_libs);

            assert_eq!(
                root["repositories"],
                json!([{ "type": "path", "url": "../../../libs/php/core" }])
            );
        }

        #[test]
        fn removes_the_repository_and_requirement_of_a_dropped_dependency() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));

            sync_manifest(&mut root, &api_packages(), &settings(), &resolve_libs);

            let changes = sync_manifest(
                &mut root,
                &[prod("lychen/core")],
                &settings(),
                &resolve_libs,
            );

            assert_eq!(root["require"], json!({ "lychen/core": "*" }));
            assert_eq!(root["require-dev"], json!({}));
            assert_eq!(root["repositories"].as_array().unwrap().len(), 1);
            assert!(
                changes.contains(&"Removed the path repository ../../../libs/php/model".to_owned())
            );
        }

        #[test]
        fn reports_the_packages_of_the_repositories_it_changed() {
            let mut root = manifest(json!({ "name": "lychen/tera-api" }));
            let packages = vec![prod("lychen/core"), package("lychen/model", None)];

            let report =
                crate::manifest::sync_manifest(&mut root, &packages, &settings(), &resolve_libs);

            assert_eq!(report.packages, ["lychen/core", "lychen/model"]);

            let report = crate::manifest::sync_manifest(
                &mut root,
                &[prod("lychen/core")],
                &settings(),
                &resolve_libs,
            );

            assert_eq!(report.packages, ["lychen/model"]);
        }

        #[test]
        fn keeps_a_repository_outside_the_workspace() {
            let mut root = manifest(json!({
                "name": "lychen/tera-api",
                "require": { "acme/patched": "*" },
                "repositories": [
                    { "type": "path", "url": "../patches/acme" },
                    { "type": "composer", "url": "https://packages.example.com" }
                ]
            }));

            sync_manifest(
                &mut root,
                &[prod("lychen/core")],
                &settings(),
                &resolve_libs,
            );

            assert_eq!(root["require"]["acme/patched"], "*");
            assert_eq!(root["repositories"].as_array().unwrap().len(), 3);
        }

        #[test]
        fn reuses_a_repository_written_differently() {
            let mut root = manifest(json!({
                "name": "lychen/tera-api",
                "repositories": [
                    { "type": "path", "url": "./../../../libs/php/core/" }
                ]
            }));

            sync_manifest(
                &mut root,
                &[prod("lychen/core")],
                &settings(),
                &resolve_libs,
            );

            assert_eq!(root["repositories"].as_array().unwrap().len(), 1);
            assert_eq!(root["repositories"][0]["url"], "./../../../libs/php/core/");
        }
    }

    mod paths {
        use super::*;

        #[test]
        fn relative_path_between_projects() {
            assert_eq!(
                relative_path("projects/tera/api", "libs/php/core"),
                "../../../libs/php/core"
            );
            assert_eq!(relative_path("libs/php/test", "libs/php/core"), "../core");
            assert_eq!(relative_path(".", "libs/php/core"), "libs/php/core");
            assert_eq!(relative_path("", "libs/php/core"), "libs/php/core");
            assert_eq!(relative_path("libs/php/core", "."), "../../..");
            assert_eq!(relative_path("libs/php/core", "libs/php/core"), ".");
            assert_eq!(relative_path("apps\\api", "libs\\core"), "../../libs/core");
        }

        #[test]
        fn resolves_the_url_of_a_path_repository() {
            assert_eq!(
                resolve_workspace_path("projects/tera/api", "../../../libs/php/core"),
                Some("libs/php/core".into())
            );
            assert_eq!(
                resolve_workspace_path("projects/tera/api", "./packages/core/"),
                Some("projects/tera/api/packages/core".into())
            );
            assert_eq!(
                resolve_workspace_path("projects/tera/api", "../../.."),
                Some("".into())
            );
        }

        #[test]
        fn ignores_a_url_that_is_not_a_workspace_path() {
            for url in [
                "",
                "../../../../outside",
                "/var/packages/core",
                "~/packages/core",
                "../packages/*",
                "C:\\packages\\core",
                "https://example.com/core.git",
            ] {
                assert_eq!(resolve_workspace_path("projects/tera/api", url), None);
            }
        }
    }

    mod formatting {
        use super::*;

        const TWO_SPACES: &str = "{\n  \"name\": \"lychen/core\",\n  \"require\": {\n    \"php\": \"^8.4\"\n  },\n  \"keywords\": [],\n  \"extra\": {}\n}\n";

        fn parse(content: &str) -> JsonMap {
            serde_json::from_str(content).unwrap()
        }

        #[test]
        fn detects_the_indentation() {
            assert_eq!(detect_indent(TWO_SPACES), "  ");
            assert_eq!(detect_indent("{\n    \"name\": \"a/b\"\n}"), "    ");
            assert_eq!(detect_indent("{\n\t\"name\": \"a/b\"\n}"), "\t");
            assert_eq!(detect_indent("{}"), "    ");
        }

        #[test]
        fn writes_back_what_it_read() {
            assert_eq!(format_manifest(&parse(TWO_SPACES), TWO_SPACES), TWO_SPACES);

            let four_spaces = TWO_SPACES.replace("  ", "    ");
            assert_eq!(
                format_manifest(&parse(&four_spaces), &four_spaces),
                four_spaces
            );

            let tabs = TWO_SPACES.replace("  ", "\t");
            assert_eq!(format_manifest(&parse(&tabs), &tabs), tabs);

            let no_final_newline = TWO_SPACES.trim_end();
            assert_eq!(
                format_manifest(&parse(no_final_newline), no_final_newline),
                no_final_newline
            );

            let windows = TWO_SPACES.replace('\n', "\r\n");
            assert_eq!(format_manifest(&parse(&windows), &windows), windows);
        }

        #[test]
        fn keeps_slashes_and_unicode_unescaped() {
            let content = "{\n    \"description\": \"Écosystème lychen/core\"\n}\n";

            assert_eq!(format_manifest(&parse(content), content), content);
        }
    }
}
