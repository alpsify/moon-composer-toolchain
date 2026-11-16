# moon Composer toolchain

A [moon](https://moonrepo.dev/moon) toolchain plugin that keeps the `composer.json` of the PHP
projects of a monorepo in sync with the project graph, the way the built-in JavaScript and
TypeScript toolchains maintain `package.json` and `tsconfig.json`.

The `dependsOn` of a project's `moon.yml` is the source of truth. From it, the plugin maintains:

- `require` and `require-dev`: one entry per workspace package the project depends on;
- `repositories`: the `path` repositories Composer needs to find those packages.

It only edits manifests. It never runs PHP or Composer, so it fits a setup where both live in
Docker.

## Install

Enable the toolchain in `.moon/toolchains.yml`:

```yaml
composer:
  plugin: github://alpsify/moon-composer-toolchain@v0.1.0
```

moon downloads the `.wasm` attached to that
[release](https://github.com/alpsify/moon-composer-toolchain/releases) and caches it.
Without the `@v0.1.0`, it follows the latest release instead. In CI, set `GITHUB_TOKEN` so that
the download doesn't run into GitHub's rate limits.

The plugin can also be vendored: download `composer_toolchain.wasm` from a release (or
[build](#build) it), commit it in the monorepo, and point to it with a path relative to the
`.moon` directory:

```yaml
composer:
  plugin: file://./plugins/composer_toolchain.wasm
```

The toolchain applies to every project with `language: php`, and to any project that has a
`composer.json`. Projects without a `composer.json` are skipped.

Tested with moon 2.5.4 and 2.6.5.

## What a sync does

The sync runs with `moon sync projects`, and before any task of a project runs (`moon run`,
`moon check`, `moon ci`). Given:

```yaml
# projects/tera/api/moon.yml
language: php
dependsOn:
  - lychen-php-core
  - lychen-php-model
  - id: lychen-php-test
    scope: development
```

and the settings of the [shared repository](#one-shared-repository) example below, the plugin
writes to `projects/tera/api/composer.json`:

```json
{
  "require": {
    "lychen/core": "*",
    "lychen/model": "*"
  },
  "require-dev": {
    "lychen/test": "*"
  },
  "repositories": [
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
  ]
}
```

The rules:

- **The package name** is the `name` of the dependency's own `composer.json`. A dependency
  without a `composer.json` (a frontend, a Docker service) is ignored.
- **The scope decides the section.** `production` (the default) and `peer` go to `require`,
  `development` goes to `require-dev`. A dependency that changes scope moves, with its constraint.
  `build` is ignored: it is the scope moon gives to the projects a task depends on, which are not
  package relationships.
- **Constraints are yours.** A new requirement gets `dependencyVersionConstraint`; one that is
  already there keeps the constraint it has.
- **Removing a `dependsOn` removes the package** from `require`, `require-dev` and
  `repositories`. Only workspace packages are ever removed: the plugin recognizes them from the
  `repositories` it maintains, and never touches a requirement like `symfony/console`.
- **Transitive packages are registered.** Composer reads `repositories` from the root package
  only, so a package required by one of the project's dependencies is added to `repositories` as
  well, but not to `require`.
- **The file keeps its shape**: indentation, line endings and key order are preserved. With
  `config.sort-packages`, a new requirement is inserted where Composer would sort it.
- **Nothing is written when nothing changes.**

When a manifest changes, moon logs a warning that lists the changes. The plugin can't run
Composer, so updating `composer.lock` afterwards is up to you, and the warning gives the command:
`composer update` followed by the packages that changed (`composer update lychen/tiptap`). Naming
them matters. `composer update --lock` fails after a removal, as the package stays pinned to a
locked version that its repository no longer provides, and a bare `composer update` upgrades
every dependency of the project.

In CI, moon flags a sync that changed files; `moon sync projects && git diff --exit-code` turns
that into a failing check.

## Settings

In `.moon/toolchains.yml`, under `composer`:

| Setting                            | Default   | Description                                                                                                                                           |
| ---------------------------------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `syncProjectWorkspaceDependencies` | `true`    | Sync `dependsOn` to `require` and `require-dev`.                                                                                                      |
| `dependencyVersionConstraint`      | `'*'`     | Constraint of a requirement added by the sync.                                                                                                        |
| `pruneWorkspaceDependencies`       | `true`    | Remove the workspace packages that are no longer in `dependsOn`.                                                                                      |
| `syncPathRepositories`             | `true`    | Sync the `path` repositories to `repositories`.                                                                                                       |
| `pathRepositoryUrl`                | _(unset)_ | URL of a single `path` repository holding every workspace package, relative to each project. Unset: one repository per package.                       |
| `pathRepositorySymlink`            | _(unset)_ | Value of `options.symlink`. Unset: not written, Composer decides.                                                                                     |
| `pathRepositoryVersion`            | `'0.0.0'` | Version given to the packages in `options.versions`, unless their `composer.json` declares a `version`. Empty: no version is written.                 |

`moon toolchain info composer github://alpsify/moon-composer-toolchain` prints them.

Every setting can be overridden for one project, in its `moon.yml`:

```yaml
# A library that should declare what it requires, but has no use for repositories
toolchains:
  composer:
    syncPathRepositories: false
```

`toolchains: { composer: false }` disables the toolchain for a project: its manifest is left
alone, and it is no longer synced as a dependency of the others.

### One shared repository

For a workspace whose packages are copied next to each application before Composer runs (a
Docker build context, for instance):

```yaml
composer:
  plugin: github://alpsify/moon-composer-toolchain@v0.1.0
  pathRepositoryUrl: ../packages/*
  pathRepositorySymlink: false
```

The plugin maintains the `options.versions` of that one repository, which is what gives the
packages a stable version to resolve.

### One repository per package

Without `pathRepositoryUrl`, each package gets its own repository, pointing to where it lives in
the workspace:

```yaml
composer:
  plugin: github://alpsify/moon-composer-toolchain@v0.1.0
  pathRepositorySymlink: true
```

```json
{
  "repositories": [
    {
      "type": "path",
      "url": "../../../libs/php/core",
      "options": {
        "symlink": true,
        "versions": {
          "lychen/core": "0.0.0"
        }
      }
    }
  ]
}
```

In this mode, a `path` repository counts as a workspace package, and can be removed by the sync,
when it points to a directory of the workspace that holds a moon project config (`moon.yml`…)
and a `composer.json`. Any other repository is left alone.

## Build

Docker is the only requirement:

```bash
scripts/build.sh        # builds dist/composer_toolchain.wasm
scripts/build.sh test   # runs the unit tests
```

With a local Rust toolchain: `cargo build --release --target wasm32-wasip1` and
`cargo test --no-default-features`.

The code is split in two:

- `src/manifest.rs` holds all the `composer.json` logic. It has no dependency on moon and is
  unit tested natively.
- `src/tier1.rs` is the plugin itself: the functions moon calls (`register_toolchain`,
  `define_toolchain_config`, `sync_project`), which read the project graph and the files.

The moon crates are pinned in `Cargo.toml` to the versions moon 2.5 was built with, and
`Cargo.lock` comes from moon's own, since the host and the plugin exchange those types as JSON.
moon's plugin API is still marked as unstable: after a moon upgrade, run `moon sync projects`
once and check that it reports nothing unexpected.

## Release

Bump `version` in `Cargo.toml`, then tag the commit with the same version and push the tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The `Release` workflow builds the plugin and publishes a GitHub release with the `.wasm`
attached, which is what the `github://` locator downloads.

## License

[MIT](LICENSE)
