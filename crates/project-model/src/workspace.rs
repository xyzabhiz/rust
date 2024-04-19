//! Handles lowering of build-system specific workspace information (`cargo
//! metadata` or `rust-project.json`) into representation stored in the salsa
//! database -- `CrateGraph`.

use std::{collections::VecDeque, fmt, fs, iter, sync};

use anyhow::{format_err, Context};
use base_db::{
    CrateDisplayName, CrateGraph, CrateId, CrateName, CrateOrigin, Dependency, Env, FileId,
    LangCrateOrigin, ProcMacroPaths, TargetLayoutLoadResult,
};
use cfg::{CfgAtom, CfgDiff, CfgOptions};
use paths::{AbsPath, AbsPathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use semver::Version;
use span::Edition;
use stdx::always;
use toolchain::Tool;
use triomphe::Arc;

use crate::{
    build_scripts::BuildScriptOutput,
    cargo_workspace::{DepKind, PackageData, RustLibSource},
    cfg::{CfgFlag, CfgOverrides},
    env::{cargo_config_env, inject_cargo_env, inject_cargo_package_env, inject_rustc_tool_env},
    project_json::{Crate, CrateArrayIdx},
    rustc_cfg::{self, RustcCfgConfig},
    sysroot::{SysrootCrate, SysrootMode},
    target_data_layout::{self, RustcDataLayoutConfig},
    utf8_stdout, CargoConfig, CargoWorkspace, InvocationStrategy, ManifestPath, Package,
    ProjectJson, ProjectManifest, Sysroot, TargetData, TargetKind, WorkspaceBuildScripts,
};

pub type FileLoader<'a> = &'a mut dyn for<'b> FnMut(&'b AbsPath) -> Option<FileId>;

/// `PackageRoot` describes a package root folder.
/// Which may be an external dependency, or a member of
/// the current workspace.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PackageRoot {
    /// Is from the local filesystem and may be edited
    pub is_local: bool,
    pub include: Vec<AbsPathBuf>,
    pub exclude: Vec<AbsPathBuf>,
}

#[derive(Clone)]
pub enum ProjectWorkspace {
    /// Project workspace was discovered by running `cargo metadata` and `rustc --print sysroot`.
    Cargo {
        /// The workspace as returned by `cargo metadata`.
        cargo: CargoWorkspace,
        /// The build script results for the workspace.
        build_scripts: WorkspaceBuildScripts,
        /// The sysroot loaded for this workspace.
        sysroot: Result<Sysroot, Option<String>>,
        /// The rustc workspace loaded for this workspace. An `Err(None)` means loading has been
        /// disabled or was otherwise not requested.
        rustc: Result<Box<(CargoWorkspace, WorkspaceBuildScripts)>, Option<String>>,
        /// Holds cfg flags for the current target. We get those by running
        /// `rustc --print cfg`.
        // FIXME: make this a per-crate map, as, eg, build.rs might have a
        // different target.
        rustc_cfg: Vec<CfgFlag>,
        /// A set of cfg overrides for this workspace.
        cfg_overrides: CfgOverrides,
        /// The toolchain version used by this workspace.
        toolchain: Option<Version>,
        /// The target data layout queried for workspace.
        target_layout: TargetLayoutLoadResult,
        /// Environment variables set in the `.cargo/config` file.
        cargo_config_extra_env: FxHashMap<String, String>,
    },
    /// Project workspace was manually specified using a `rust-project.json` file.
    Json {
        /// The loaded project json file.
        project: ProjectJson,
        /// The sysroot loaded for this workspace.
        sysroot: Result<Sysroot, Option<String>>,
        /// Holds cfg flags for the current target. We get those by running
        /// `rustc --print cfg`.
        // FIXME: make this a per-crate map, as, eg, build.rs might have a
        // different target.
        rustc_cfg: Vec<CfgFlag>,
        /// The toolchain version used by this workspace.
        toolchain: Option<Version>,
        /// The target data layout queried for workspace.
        target_layout: TargetLayoutLoadResult,
        /// A set of cfg overrides for this workspace.
        cfg_overrides: CfgOverrides,
    },
    // FIXME: The primary limitation of this approach is that the set of detached files needs to be fixed at the beginning.
    // That's not the end user experience we should strive for.
    // Ideally, you should be able to just open a random detached file in existing cargo projects, and get the basic features working.
    // That needs some changes on the salsa-level though.
    // In particular, we should split the unified CrateGraph (which currently has maximal durability) into proper crate graph, and a set of ad hoc roots (with minimal durability).
    // Then, we need to hide the graph behind the queries such that most queries look only at the proper crate graph, and fall back to ad hoc roots only if there's no results.
    // After this, we should be able to tweak the logic in reload.rs to add newly opened files, which don't belong to any existing crates, to the set of the detached files.
    // //
    /// Project with a set of disjoint files, not belonging to any particular workspace.
    /// Backed by basic sysroot crates for basic completion and highlighting.
    DetachedFile {
        /// The file in question.
        file: AbsPathBuf,
        /// The sysroot loaded for this workspace.
        sysroot: Result<Sysroot, Option<String>>,
        /// Holds cfg flags for the current target. We get those by running
        /// `rustc --print cfg`.
        // FIXME: make this a per-crate map, as, eg, build.rs might have a
        // different target.
        rustc_cfg: Vec<CfgFlag>,
        /// The toolchain version used by this workspace.
        toolchain: Option<Version>,
        /// The target data layout queried for workspace.
        target_layout: TargetLayoutLoadResult,
        /// A set of cfg overrides for the files.
        cfg_overrides: CfgOverrides,
        /// Is this file a cargo script file?
        cargo_script: Option<CargoWorkspace>,
    },
}

impl fmt::Debug for ProjectWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Make sure this isn't too verbose.
        match self {
            ProjectWorkspace::Cargo {
                cargo,
                build_scripts: _,
                sysroot,
                rustc,
                rustc_cfg,
                cfg_overrides,
                toolchain,
                target_layout,
                cargo_config_extra_env,
            } => f
                .debug_struct("Cargo")
                .field("root", &cargo.workspace_root().file_name())
                .field("n_packages", &cargo.packages().len())
                .field("sysroot", &sysroot.is_ok())
                .field(
                    "n_rustc_compiler_crates",
                    &rustc.as_ref().map(|a| a.as_ref()).map_or(0, |(rc, _)| rc.packages().len()),
                )
                .field("n_rustc_cfg", &rustc_cfg.len())
                .field("n_cfg_overrides", &cfg_overrides.len())
                .field("toolchain", &toolchain)
                .field("data_layout", &target_layout)
                .field("cargo_config_extra_env", &cargo_config_extra_env)
                .finish(),
            ProjectWorkspace::Json {
                project,
                sysroot,
                rustc_cfg,
                toolchain,
                target_layout: data_layout,
                cfg_overrides,
            } => {
                let mut debug_struct = f.debug_struct("Json");
                debug_struct.field("n_crates", &project.n_crates());
                if let Ok(sysroot) = sysroot {
                    debug_struct.field("n_sysroot_crates", &sysroot.num_packages());
                }
                debug_struct
                    .field("n_rustc_cfg", &rustc_cfg.len())
                    .field("toolchain", &toolchain)
                    .field("data_layout", &data_layout)
                    .field("n_cfg_overrides", &cfg_overrides.len());
                debug_struct.finish()
            }
            ProjectWorkspace::DetachedFile {
                file,
                sysroot,
                rustc_cfg,
                toolchain,
                target_layout,
                cfg_overrides,
                cargo_script,
            } => f
                .debug_struct("DetachedFiles")
                .field("file", &file)
                .field("cargo_script", &cargo_script.is_some())
                .field("sysroot", &sysroot.is_ok())
                .field("cargo_script", &cargo_script.is_some())
                .field("n_rustc_cfg", &rustc_cfg.len())
                .field("toolchain", &toolchain)
                .field("data_layout", &target_layout)
                .field("n_cfg_overrides", &cfg_overrides.len())
                .finish(),
        }
    }
}

fn get_toolchain_version(
    current_dir: &AbsPath,
    sysroot: Option<&Sysroot>,
    tool: Tool,
    extra_env: &FxHashMap<String, String>,
    prefix: &str,
) -> Result<Option<Version>, anyhow::Error> {
    let cargo_version = utf8_stdout({
        let mut cmd = Sysroot::tool(sysroot, tool);
        cmd.envs(extra_env);
        cmd.arg("--version").current_dir(current_dir);
        cmd
    })
    .with_context(|| format!("Failed to query rust toolchain version at {current_dir}, is your toolchain setup correctly?"))?;
    anyhow::Ok(
        cargo_version
            .get(prefix.len()..)
            .and_then(|it| Version::parse(it.split_whitespace().next()?).ok()),
    )
}

impl ProjectWorkspace {
    pub fn load(
        manifest: ProjectManifest,
        config: &CargoConfig,
        progress: &dyn Fn(String),
    ) -> anyhow::Result<ProjectWorkspace> {
        ProjectWorkspace::load_inner(&manifest, config, progress)
            .with_context(|| format!("Failed to load the project at {manifest}"))
    }

    fn load_inner(
        manifest: &ProjectManifest,
        config: &CargoConfig,
        progress: &dyn Fn(String),
    ) -> anyhow::Result<ProjectWorkspace> {
        let res = match manifest {
            ProjectManifest::ProjectJson(project_json) => {
                let file = fs::read_to_string(project_json)
                    .with_context(|| format!("Failed to read json file {project_json}"))?;
                let data = serde_json::from_str(&file)
                    .with_context(|| format!("Failed to deserialize json file {project_json}"))?;
                let project_location = project_json.parent().to_path_buf();
                let project_json: ProjectJson = ProjectJson::new(&project_location, data);
                ProjectWorkspace::load_inline(
                    project_json,
                    config.target.as_deref(),
                    &config.extra_env,
                    &config.cfg_overrides,
                )
            }
            ProjectManifest::CargoToml(cargo_toml) => {
                let sysroot = match (&config.sysroot, &config.sysroot_src) {
                    (Some(RustLibSource::Path(path)), None) => {
                        Sysroot::with_sysroot_dir(path.clone(), config.sysroot_query_metadata).map_err(|e| {
                          Some(format!("Failed to find sysroot at {path}:{e}"))
                        })
                    }
                    (Some(RustLibSource::Discover), None) => {
                        Sysroot::discover(cargo_toml.parent(), &config.extra_env, config.sysroot_query_metadata).map_err(|e| {
                            Some(format!("Failed to find sysroot for Cargo.toml file {cargo_toml}. Is rust-src installed? {e}"))
                        })
                    }
                    (Some(RustLibSource::Path(sysroot)), Some(sysroot_src)) => {
                        Ok(Sysroot::load(sysroot.clone(), Some(Ok(sysroot_src.clone())), config.sysroot_query_metadata))
                    }
                    (Some(RustLibSource::Discover), Some(sysroot_src)) => {
                        Sysroot::discover_with_src_override(
                            cargo_toml.parent(),
                            &config.extra_env,
                            sysroot_src.clone(), config.sysroot_query_metadata,
                        ).map_err(|e| {
                            Some(format!("Failed to find sysroot for Cargo.toml file {cargo_toml}. Is rust-src installed? {e}"))
                        })
                    }
                    (None, _) => Err(None),
                };
                let sysroot_ref = sysroot.as_ref().ok();

                if let Ok(sysroot) = &sysroot {
                    tracing::info!(workspace = %cargo_toml, src_root = ?sysroot.src_root(), root = %sysroot.root(), "Using sysroot");
                }

                let rustc_dir = match &config.rustc_source {
                    Some(RustLibSource::Path(path)) => ManifestPath::try_from(path.clone())
                        .map_err(|p| Some(format!("rustc source path is not absolute: {p}"))),
                    Some(RustLibSource::Discover) => {
                        sysroot_ref.and_then(Sysroot::discover_rustc_src).ok_or_else(|| {
                            Some("Failed to discover rustc source for sysroot.".to_owned())
                        })
                    }
                    None => Err(None),
                };

                let rustc =  rustc_dir.and_then(|rustc_dir| {
                    tracing::info!(workspace = %cargo_toml, rustc_dir = %rustc_dir, "Using rustc source");
                    match CargoWorkspace::fetch_metadata(
                        &rustc_dir,
                        cargo_toml.parent(),
                        &CargoConfig {
                            features: crate::CargoFeatures::default(),
                            ..config.clone()
                        },
                        sysroot_ref,
                        progress,
                    ) {
                        Ok(meta) => {
                            let workspace = CargoWorkspace::new(meta);
                            let buildscripts = WorkspaceBuildScripts::rustc_crates(
                                &workspace,
                                cargo_toml.parent(),
                                &config.extra_env,
                                sysroot_ref
                            );
                            Ok(Box::new((workspace, buildscripts)))
                        }
                        Err(e) => {
                            tracing::error!(
                                %e,
                                "Failed to read Cargo metadata from rustc source at {rustc_dir}",
                            );
                            Err(Some(format!(
                                "Failed to read Cargo metadata from rustc source at {rustc_dir}: {e}"
                            )))
                        }
                    }
                });

                let toolchain = get_toolchain_version(
                    cargo_toml.parent(),
                    sysroot_ref,
                    Tool::Cargo,
                    &config.extra_env,
                    "cargo ",
                )?;
                let rustc_cfg = rustc_cfg::get(
                    config.target.as_deref(),
                    &config.extra_env,
                    RustcCfgConfig::Cargo(sysroot_ref, cargo_toml),
                );

                let cfg_overrides = config.cfg_overrides.clone();
                let data_layout = target_data_layout::get(
                    RustcDataLayoutConfig::Cargo(sysroot_ref, cargo_toml),
                    config.target.as_deref(),
                    &config.extra_env,
                );
                if let Err(e) = &data_layout {
                    tracing::error!(%e, "failed fetching data layout for {cargo_toml:?} workspace");
                }

                let meta = CargoWorkspace::fetch_metadata(
                    cargo_toml,
                    cargo_toml.parent(),
                    config,
                    sysroot_ref,
                    progress,
                )
                .with_context(|| {
                    format!(
                        "Failed to read Cargo metadata from Cargo.toml file {cargo_toml}, {toolchain:?}",
                    )
                })?;
                let cargo = CargoWorkspace::new(meta);

                let cargo_config_extra_env =
                    cargo_config_env(cargo_toml, &config.extra_env, sysroot_ref);
                ProjectWorkspace::Cargo {
                    cargo,
                    build_scripts: WorkspaceBuildScripts::default(),
                    sysroot,
                    rustc,
                    rustc_cfg,
                    cfg_overrides,
                    toolchain,
                    target_layout: data_layout
                        .map(Arc::from)
                        .map_err(|it| Arc::from(it.to_string())),
                    cargo_config_extra_env,
                }
            }
        };

        Ok(res)
    }

    pub fn load_inline(
        project_json: ProjectJson,
        target: Option<&str>,
        extra_env: &FxHashMap<String, String>,
        cfg_overrides: &CfgOverrides,
    ) -> ProjectWorkspace {
        let sysroot = match (project_json.sysroot.clone(), project_json.sysroot_src.clone()) {
            (Some(sysroot), Some(sysroot_src)) => {
                Ok(Sysroot::load(sysroot, Some(Ok(sysroot_src)), false))
            }
            (Some(sysroot), None) => {
                // assume sysroot is structured like rustup's and guess `sysroot_src`
                let sysroot_src =
                    sysroot.join("lib").join("rustlib").join("src").join("rust").join("library");
                Ok(Sysroot::load(sysroot, Some(Ok(sysroot_src)), false))
            }
            (None, Some(sysroot_src)) => {
                // assume sysroot is structured like rustup's and guess `sysroot`
                let mut sysroot = sysroot_src.clone();
                for _ in 0..5 {
                    sysroot.pop();
                }
                Ok(Sysroot::load(sysroot, Some(Ok(sysroot_src)), false))
            }
            (None, None) => Err(None),
        };
        let sysroot_ref = sysroot.as_ref().ok();
        let cfg_config = RustcCfgConfig::Rustc(sysroot_ref);
        let data_layout_config = RustcDataLayoutConfig::Rustc(sysroot_ref);
        let toolchain = match get_toolchain_version(
            project_json.path(),
            sysroot_ref,
            Tool::Rustc,
            extra_env,
            "rustc ",
        ) {
            Ok(it) => it,
            Err(e) => {
                tracing::error!("{e}");
                None
            }
        };

        let rustc_cfg = rustc_cfg::get(target, extra_env, cfg_config);
        let data_layout = target_data_layout::get(data_layout_config, target, extra_env);
        ProjectWorkspace::Json {
            project: project_json,
            sysroot,
            rustc_cfg,
            toolchain,
            target_layout: data_layout.map(Arc::from).map_err(|it| Arc::from(it.to_string())),
            cfg_overrides: cfg_overrides.clone(),
        }
    }

    pub fn load_detached_files(
        detached_files: Vec<AbsPathBuf>,
        config: &CargoConfig,
    ) -> Vec<anyhow::Result<ProjectWorkspace>> {
        detached_files
            .into_iter()
            .map(|detached_file| {
                let dir = detached_file
                    .parent()
                    .ok_or_else(|| format_err!("detached file has no parent"))?;
                let sysroot = match &config.sysroot {
                    Some(RustLibSource::Path(path)) => {
                        Sysroot::with_sysroot_dir(path.clone(), config.sysroot_query_metadata)
                            .map_err(|e| Some(format!("Failed to find sysroot at {path}:{e}")))
                    }
                    Some(RustLibSource::Discover) => {
                        Sysroot::discover(dir, &config.extra_env, config.sysroot_query_metadata)
                            .map_err(|e| {
                                Some(format!(
                                    "Failed to find sysroot for {dir}. Is rust-src installed? {e}"
                                ))
                            })
                    }
                    None => Err(None),
                };

                let sysroot_ref = sysroot.as_ref().ok();
                let toolchain = match get_toolchain_version(
                    dir,
                    sysroot_ref,
                    Tool::Rustc,
                    &config.extra_env,
                    "rustc ",
                ) {
                    Ok(it) => it,
                    Err(e) => {
                        tracing::error!("{e}");
                        None
                    }
                };

                let rustc_cfg =
                    rustc_cfg::get(None, &config.extra_env, RustcCfgConfig::Rustc(sysroot_ref));
                let data_layout = target_data_layout::get(
                    RustcDataLayoutConfig::Rustc(sysroot_ref),
                    None,
                    &config.extra_env,
                );

                let cargo_script = ManifestPath::try_from(detached_file.clone())
                    .ok()
                    .and_then(|file| {
                        CargoWorkspace::fetch_metadata(
                            &file,
                            file.parent(),
                            config,
                            sysroot_ref,
                            &|_| (),
                        )
                        .ok()
                    })
                    .map(CargoWorkspace::new);

                Ok(ProjectWorkspace::DetachedFile {
                    file: detached_file,
                    sysroot,
                    rustc_cfg,
                    toolchain,
                    target_layout: data_layout
                        .map(Arc::from)
                        .map_err(|it| Arc::from(it.to_string())),
                    cfg_overrides: config.cfg_overrides.clone(),
                    cargo_script,
                })
            })
            .collect()
    }

    /// Runs the build scripts for this [`ProjectWorkspace`].
    pub fn run_build_scripts(
        &self,
        config: &CargoConfig,
        progress: &dyn Fn(String),
    ) -> anyhow::Result<WorkspaceBuildScripts> {
        match self {
            ProjectWorkspace::Cargo { cargo, toolchain, sysroot, .. } => {
                WorkspaceBuildScripts::run_for_workspace(
                    config,
                    cargo,
                    progress,
                    toolchain,
                    sysroot.as_ref().ok(),
                )
                .with_context(|| {
                    format!("Failed to run build scripts for {}", cargo.workspace_root())
                })
            }
            ProjectWorkspace::Json { .. } | ProjectWorkspace::DetachedFile { .. } => {
                Ok(WorkspaceBuildScripts::default())
            }
        }
    }

    /// Runs the build scripts for the given [`ProjectWorkspace`]s. Depending on the invocation
    /// strategy this may run a single build process for all project workspaces.
    pub fn run_all_build_scripts(
        workspaces: &[ProjectWorkspace],
        config: &CargoConfig,
        progress: &dyn Fn(String),
        workspace_root: &AbsPathBuf,
    ) -> Vec<anyhow::Result<WorkspaceBuildScripts>> {
        if matches!(config.invocation_strategy, InvocationStrategy::PerWorkspace)
            || config.run_build_script_command.is_none()
        {
            return workspaces.iter().map(|it| it.run_build_scripts(config, progress)).collect();
        }

        let cargo_ws: Vec<_> = workspaces
            .iter()
            .filter_map(|it| match it {
                ProjectWorkspace::Cargo { cargo, .. } => Some(cargo),
                _ => None,
            })
            .collect();
        let outputs =
            &mut match WorkspaceBuildScripts::run_once(config, &cargo_ws, progress, workspace_root)
            {
                Ok(it) => Ok(it.into_iter()),
                // io::Error is not Clone?
                Err(e) => Err(sync::Arc::new(e)),
            };

        workspaces
            .iter()
            .map(|it| match it {
                ProjectWorkspace::Cargo { cargo, .. } => match outputs {
                    Ok(outputs) => Ok(outputs.next().unwrap()),
                    Err(e) => Err(e.clone()).with_context(|| {
                        format!("Failed to run build scripts for {}", cargo.workspace_root())
                    }),
                },
                _ => Ok(WorkspaceBuildScripts::default()),
            })
            .collect()
    }

    pub fn set_build_scripts(&mut self, bs: WorkspaceBuildScripts) {
        match self {
            ProjectWorkspace::Cargo { build_scripts, .. } => *build_scripts = bs,
            _ => {
                always!(bs == WorkspaceBuildScripts::default());
            }
        }
    }

    pub fn workspace_definition_path(&self) -> &AbsPath {
        match self {
            ProjectWorkspace::Cargo { cargo, .. } => cargo.workspace_root(),
            ProjectWorkspace::Json { project, .. } => project.path(),
            ProjectWorkspace::DetachedFile { file, .. } => file,
        }
    }

    pub fn find_sysroot_proc_macro_srv(&self) -> anyhow::Result<AbsPathBuf> {
        match self {
            ProjectWorkspace::Cargo { sysroot: Ok(sysroot), .. }
            | ProjectWorkspace::Json { sysroot: Ok(sysroot), .. }
            | ProjectWorkspace::DetachedFile { sysroot: Ok(sysroot), .. } => {
                sysroot.discover_proc_macro_srv()
            }
            ProjectWorkspace::DetachedFile { .. } => {
                Err(anyhow::format_err!("cannot find proc-macro server, no sysroot was found"))
            }
            ProjectWorkspace::Cargo { cargo, .. } => Err(anyhow::format_err!(
                "cannot find proc-macro-srv, the workspace `{}` is missing a sysroot",
                cargo.workspace_root()
            )),
            ProjectWorkspace::Json { project, .. } => Err(anyhow::format_err!(
                "cannot find proc-macro-srv, the workspace `{}` is missing a sysroot",
                project.path()
            )),
        }
    }

    /// Returns the roots for the current `ProjectWorkspace`
    /// The return type contains the path and whether or not
    /// the root is a member of the current workspace
    pub fn to_roots(&self) -> Vec<PackageRoot> {
        let mk_sysroot = |sysroot: Result<_, _>| {
            sysroot.into_iter().flat_map(move |sysroot: &Sysroot| {
                let mut r = match sysroot.mode() {
                    SysrootMode::Workspace(ws) => ws
                        .packages()
                        .filter_map(|pkg| {
                            if ws[pkg].is_local {
                                // the local ones are included in the main `PackageRoot`` below
                                return None;
                            }
                            let pkg_root = ws[pkg].manifest.parent().to_path_buf();

                            let include = vec![pkg_root.clone()];

                            let exclude = vec![
                                pkg_root.join(".git"),
                                pkg_root.join("target"),
                                pkg_root.join("tests"),
                                pkg_root.join("examples"),
                                pkg_root.join("benches"),
                            ];
                            Some(PackageRoot { is_local: false, include, exclude })
                        })
                        .collect(),
                    SysrootMode::Stitched(_) => vec![],
                };

                r.push(PackageRoot {
                    is_local: false,
                    include: sysroot.src_root().map(|it| it.to_path_buf()).into_iter().collect(),
                    exclude: Vec::new(),
                });
                r
            })
        };
        match self {
            ProjectWorkspace::Json {
                project,
                sysroot,
                rustc_cfg: _,
                toolchain: _,
                target_layout: _,
                cfg_overrides: _,
            } => project
                .crates()
                .map(|(_, krate)| PackageRoot {
                    is_local: krate.is_workspace_member,
                    include: krate.include.clone(),
                    exclude: krate.exclude.clone(),
                })
                .collect::<FxHashSet<_>>()
                .into_iter()
                .chain(mk_sysroot(sysroot.as_ref()))
                .collect::<Vec<_>>(),
            ProjectWorkspace::Cargo {
                cargo,
                sysroot,
                rustc,
                rustc_cfg: _,
                cfg_overrides: _,
                build_scripts,
                toolchain: _,
                target_layout: _,
                cargo_config_extra_env: _,
            } => {
                cargo
                    .packages()
                    .map(|pkg| {
                        let is_local = cargo[pkg].is_local;
                        let pkg_root = cargo[pkg].manifest.parent().to_path_buf();

                        let mut include = vec![pkg_root.clone()];
                        let out_dir =
                            build_scripts.get_output(pkg).and_then(|it| it.out_dir.clone());
                        include.extend(out_dir);

                        // In case target's path is manually set in Cargo.toml to be
                        // outside the package root, add its parent as an extra include.
                        // An example of this situation would look like this:
                        //
                        // ```toml
                        // [lib]
                        // path = "../../src/lib.rs"
                        // ```
                        let extra_targets = cargo[pkg]
                            .targets
                            .iter()
                            .filter(|&&tgt| matches!(cargo[tgt].kind, TargetKind::Lib { .. }))
                            .filter_map(|&tgt| cargo[tgt].root.parent())
                            .map(|tgt| tgt.normalize().to_path_buf())
                            .filter(|path| !path.starts_with(&pkg_root));
                        include.extend(extra_targets);

                        let mut exclude = vec![pkg_root.join(".git")];
                        if is_local {
                            exclude.push(pkg_root.join("target"));
                        } else {
                            exclude.push(pkg_root.join("tests"));
                            exclude.push(pkg_root.join("examples"));
                            exclude.push(pkg_root.join("benches"));
                        }
                        PackageRoot { is_local, include, exclude }
                    })
                    .chain(mk_sysroot(sysroot.as_ref()))
                    .chain(rustc.iter().map(|a| a.as_ref()).flat_map(|(rustc, _)| {
                        rustc.packages().map(move |krate| PackageRoot {
                            is_local: false,
                            include: vec![rustc[krate].manifest.parent().to_path_buf()],
                            exclude: Vec::new(),
                        })
                    }))
                    .collect()
            }
            ProjectWorkspace::DetachedFile { file, sysroot, .. } => iter::once(PackageRoot {
                is_local: true,
                include: vec![file.clone()],
                exclude: Vec::new(),
            })
            .chain(mk_sysroot(sysroot.as_ref()))
            .collect(),
        }
    }

    pub fn n_packages(&self) -> usize {
        match self {
            ProjectWorkspace::Json { project, sysroot, .. } => {
                let sysroot_package_len = sysroot.as_ref().map_or(0, |it| it.num_packages());
                sysroot_package_len + project.n_crates()
            }
            ProjectWorkspace::Cargo { cargo, sysroot, rustc, .. } => {
                let rustc_package_len =
                    rustc.as_ref().map(|a| a.as_ref()).map_or(0, |(it, _)| it.packages().len());
                let sysroot_package_len = sysroot.as_ref().map_or(0, |it| it.num_packages());
                cargo.packages().len() + sysroot_package_len + rustc_package_len
            }
            ProjectWorkspace::DetachedFile { sysroot, .. } => {
                let sysroot_package_len = sysroot.as_ref().map_or(0, |it| it.num_packages());
                sysroot_package_len + 1
            }
        }
    }

    pub fn to_crate_graph(
        &self,
        load: FileLoader<'_>,
        extra_env: &FxHashMap<String, String>,
    ) -> (CrateGraph, ProcMacroPaths) {
        let _p = tracing::span!(tracing::Level::INFO, "ProjectWorkspace::to_crate_graph").entered();

        let ((mut crate_graph, proc_macros), sysroot) = match self {
            ProjectWorkspace::Json {
                project,
                sysroot,
                rustc_cfg,
                toolchain: _,
                target_layout: _,
                cfg_overrides,
            } => (
                project_json_to_crate_graph(
                    rustc_cfg.clone(),
                    load,
                    project,
                    sysroot.as_ref().ok(),
                    extra_env,
                    cfg_overrides,
                ),
                sysroot,
            ),
            ProjectWorkspace::Cargo {
                cargo,
                sysroot,
                rustc,
                rustc_cfg,
                cfg_overrides,
                build_scripts,
                toolchain: _,
                target_layout: _,
                cargo_config_extra_env: _,
            } => (
                cargo_to_crate_graph(
                    load,
                    rustc.as_ref().map(|a| a.as_ref()).ok(),
                    cargo,
                    sysroot.as_ref().ok(),
                    rustc_cfg.clone(),
                    cfg_overrides,
                    build_scripts,
                ),
                sysroot,
            ),
            ProjectWorkspace::DetachedFile {
                file,
                sysroot,
                rustc_cfg,
                toolchain: _,
                target_layout: _,
                cfg_overrides,
                cargo_script,
            } => (
                if let Some(cargo) = cargo_script {
                    cargo_to_crate_graph(
                        &mut |path| load(path),
                        None,
                        cargo,
                        sysroot.as_ref().ok(),
                        rustc_cfg.clone(),
                        cfg_overrides,
                        &WorkspaceBuildScripts::default(),
                    )
                } else {
                    detached_file_to_crate_graph(
                        rustc_cfg.clone(),
                        load,
                        file,
                        sysroot.as_ref().ok(),
                        cfg_overrides,
                    )
                },
                sysroot,
            ),
        };

        if matches!(sysroot.as_ref().map(|it| it.mode()), Ok(SysrootMode::Stitched(_)))
            && crate_graph.patch_cfg_if()
        {
            tracing::debug!("Patched std to depend on cfg-if")
        } else {
            tracing::debug!("Did not patch std to depend on cfg-if")
        }
        (crate_graph, proc_macros)
    }

    pub fn eq_ignore_build_data(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Cargo {
                    cargo,
                    sysroot,
                    rustc,
                    rustc_cfg,
                    cfg_overrides,
                    toolchain,
                    cargo_config_extra_env,
                    build_scripts: _,
                    target_layout: _,
                },
                Self::Cargo {
                    cargo: o_cargo,
                    sysroot: o_sysroot,
                    rustc: o_rustc,
                    rustc_cfg: o_rustc_cfg,
                    cfg_overrides: o_cfg_overrides,
                    toolchain: o_toolchain,
                    cargo_config_extra_env: o_cargo_config_extra_env,
                    build_scripts: _,
                    target_layout: _,
                },
            ) => {
                cargo == o_cargo
                    && rustc == o_rustc
                    && rustc_cfg == o_rustc_cfg
                    && cfg_overrides == o_cfg_overrides
                    && toolchain == o_toolchain
                    && sysroot == o_sysroot
                    && cargo_config_extra_env == o_cargo_config_extra_env
            }
            (
                Self::Json {
                    project,
                    sysroot,
                    rustc_cfg,
                    toolchain,
                    target_layout: _,
                    cfg_overrides,
                },
                Self::Json {
                    project: o_project,
                    sysroot: o_sysroot,
                    rustc_cfg: o_rustc_cfg,
                    toolchain: o_toolchain,
                    target_layout: _,
                    cfg_overrides: o_cfg_overrides,
                },
            ) => {
                project == o_project
                    && rustc_cfg == o_rustc_cfg
                    && sysroot == o_sysroot
                    && toolchain == o_toolchain
                    && cfg_overrides == o_cfg_overrides
            }
            (
                Self::DetachedFile {
                    file,
                    sysroot,
                    rustc_cfg,
                    cargo_script,
                    toolchain,
                    target_layout,
                    cfg_overrides,
                },
                Self::DetachedFile {
                    file: o_file,
                    sysroot: o_sysroot,
                    rustc_cfg: o_rustc_cfg,
                    cargo_script: o_cargo_script,
                    toolchain: o_toolchain,
                    target_layout: o_target_layout,
                    cfg_overrides: o_cfg_overrides,
                },
            ) => {
                file == o_file
                    && sysroot == o_sysroot
                    && rustc_cfg == o_rustc_cfg
                    && toolchain == o_toolchain
                    && target_layout == o_target_layout
                    && cfg_overrides == o_cfg_overrides
                    && cargo_script == o_cargo_script
            }
            _ => false,
        }
    }

    /// Returns `true` if the project workspace is [`Json`].
    ///
    /// [`Json`]: ProjectWorkspace::Json
    #[must_use]
    pub fn is_json(&self) -> bool {
        matches!(self, Self::Json { .. })
    }
}

fn project_json_to_crate_graph(
    rustc_cfg: Vec<CfgFlag>,
    load: FileLoader<'_>,
    project: &ProjectJson,
    sysroot: Option<&Sysroot>,
    extra_env: &FxHashMap<String, String>,
    override_cfg: &CfgOverrides,
) -> (CrateGraph, ProcMacroPaths) {
    let mut res = (CrateGraph::default(), ProcMacroPaths::default());
    let (crate_graph, proc_macros) = &mut res;
    let sysroot_deps = sysroot
        .as_ref()
        .map(|sysroot| sysroot_to_crate_graph(crate_graph, sysroot, rustc_cfg.clone(), load));

    let r_a_cfg_flag = CfgFlag::Atom("rust_analyzer".to_owned());
    let mut cfg_cache: FxHashMap<&str, Vec<CfgFlag>> = FxHashMap::default();

    let idx_to_crate_id: FxHashMap<CrateArrayIdx, CrateId> = project
        .crates()
        .filter_map(|(idx, krate)| Some((idx, krate, load(&krate.root_module)?)))
        .map(
            |(
                idx,
                Crate {
                    display_name,
                    edition,
                    version,
                    cfg,
                    target,
                    env,
                    proc_macro_dylib_path,
                    is_proc_macro,
                    repository,
                    ..
                },
                file_id,
            )| {
                let env = env.clone().into_iter().collect();

                let target_cfgs = match target.as_deref() {
                    Some(target) => cfg_cache.entry(target).or_insert_with(|| {
                        rustc_cfg::get(Some(target), extra_env, RustcCfgConfig::Rustc(sysroot))
                    }),
                    None => &rustc_cfg,
                };

                let mut cfg_options = target_cfgs
                    .iter()
                    .chain(cfg.iter())
                    .chain(iter::once(&r_a_cfg_flag))
                    .cloned()
                    .collect();
                override_cfg.apply(
                    &mut cfg_options,
                    display_name.as_ref().map(|it| it.canonical_name()).unwrap_or_default(),
                );
                let crate_graph_crate_id = crate_graph.add_crate_root(
                    file_id,
                    *edition,
                    display_name.clone(),
                    version.clone(),
                    Arc::new(cfg_options),
                    None,
                    env,
                    *is_proc_macro,
                    if let Some(name) = display_name.clone() {
                        CrateOrigin::Local {
                            repo: repository.clone(),
                            name: Some(name.canonical_name().to_owned()),
                        }
                    } else {
                        CrateOrigin::Local { repo: None, name: None }
                    },
                );
                if *is_proc_macro {
                    if let Some(path) = proc_macro_dylib_path.clone() {
                        let node = Ok((
                            display_name.as_ref().map(|it| it.canonical_name().to_owned()),
                            path,
                        ));
                        proc_macros.insert(crate_graph_crate_id, node);
                    }
                }
                (idx, crate_graph_crate_id)
            },
        )
        .collect();

    for (from_idx, krate) in project.crates() {
        if let Some(&from) = idx_to_crate_id.get(&from_idx) {
            if let Some((public_deps, libproc_macro)) = &sysroot_deps {
                public_deps.add_to_crate_graph(crate_graph, from);
                if let Some(proc_macro) = libproc_macro {
                    add_proc_macro_dep(crate_graph, from, *proc_macro, krate.is_proc_macro);
                }
            }

            for dep in &krate.deps {
                if let Some(&to) = idx_to_crate_id.get(&dep.krate) {
                    add_dep(crate_graph, from, dep.name.clone(), to);
                }
            }
        }
    }
    res
}

fn cargo_to_crate_graph(
    load: FileLoader<'_>,
    rustc: Option<&(CargoWorkspace, WorkspaceBuildScripts)>,
    cargo: &CargoWorkspace,
    sysroot: Option<&Sysroot>,
    rustc_cfg: Vec<CfgFlag>,
    override_cfg: &CfgOverrides,
    build_scripts: &WorkspaceBuildScripts,
) -> (CrateGraph, ProcMacroPaths) {
    let _p = tracing::span!(tracing::Level::INFO, "cargo_to_crate_graph").entered();
    let mut res = (CrateGraph::default(), ProcMacroPaths::default());
    let crate_graph = &mut res.0;
    let proc_macros = &mut res.1;
    let (public_deps, libproc_macro) = match sysroot {
        Some(sysroot) => sysroot_to_crate_graph(crate_graph, sysroot, rustc_cfg.clone(), load),
        None => (SysrootPublicDeps::default(), None),
    };

    let cfg_options = CfgOptions::from_iter(rustc_cfg);

    // Mapping of a package to its library target
    let mut pkg_to_lib_crate = FxHashMap::default();
    let mut pkg_crates = FxHashMap::default();
    // Does any crate signal to rust-analyzer that they need the rustc_private crates?
    let mut has_private = false;

    // Next, create crates for each package, target pair
    for pkg in cargo.packages() {
        has_private |= cargo[pkg].metadata.rustc_private;

        let cfg_options = {
            let mut cfg_options = cfg_options.clone();

            if cargo[pkg].is_local {
                // Add test cfg for local crates
                cfg_options.insert_atom("test".into());
                cfg_options.insert_atom("rust_analyzer".into());
            }

            override_cfg.apply(&mut cfg_options, &cargo[pkg].name);
            cfg_options
        };

        let mut lib_tgt = None;
        for &tgt in cargo[pkg].targets.iter() {
            if !matches!(cargo[tgt].kind, TargetKind::Lib { .. }) && !cargo[pkg].is_member {
                // For non-workspace-members, Cargo does not resolve dev-dependencies, so we don't
                // add any targets except the library target, since those will not work correctly if
                // they use dev-dependencies.
                // In fact, they can break quite badly if multiple client workspaces get merged:
                // https://github.com/rust-lang/rust-analyzer/issues/11300
                continue;
            }
            let &TargetData { ref name, kind, ref root, .. } = &cargo[tgt];

            let Some(file_id) = load(root) else { continue };

            let build_data = build_scripts.get_output(pkg);
            let pkg_data = &cargo[pkg];
            let crate_id = add_target_crate_root(
                crate_graph,
                proc_macros,
                pkg_data,
                build_data,
                cfg_options.clone(),
                file_id,
                name,
                kind,
                if pkg_data.is_local {
                    CrateOrigin::Local {
                        repo: pkg_data.repository.clone(),
                        name: Some(pkg_data.name.clone()),
                    }
                } else {
                    CrateOrigin::Library {
                        repo: pkg_data.repository.clone(),
                        name: pkg_data.name.clone(),
                    }
                },
            );
            if let TargetKind::Lib { .. } = kind {
                lib_tgt = Some((crate_id, name.clone()));
                pkg_to_lib_crate.insert(pkg, crate_id);
            }
            // Even crates that don't set proc-macro = true are allowed to depend on proc_macro
            // (just none of the APIs work when called outside of a proc macro).
            if let Some(proc_macro) = libproc_macro {
                add_proc_macro_dep(
                    crate_graph,
                    crate_id,
                    proc_macro,
                    matches!(kind, TargetKind::Lib { is_proc_macro: true }),
                );
            }

            pkg_crates.entry(pkg).or_insert_with(Vec::new).push((crate_id, kind));
        }

        // Set deps to the core, std and to the lib target of the current package
        for &(from, kind) in pkg_crates.get(&pkg).into_iter().flatten() {
            // Add sysroot deps first so that a lib target named `core` etc. can overwrite them.
            public_deps.add_to_crate_graph(crate_graph, from);

            // Add dep edge of all targets to the package's lib target
            if let Some((to, name)) = lib_tgt.clone() {
                if to != from && kind != TargetKind::BuildScript {
                    // (build script can not depend on its library target)

                    // For root projects with dashes in their name,
                    // cargo metadata does not do any normalization,
                    // so we do it ourselves currently
                    let name = CrateName::normalize_dashes(&name);
                    add_dep(crate_graph, from, name, to);
                }
            }
        }
    }

    let mut delayed_dev_deps = vec![];

    // Now add a dep edge from all targets of upstream to the lib
    // target of downstream.
    for pkg in cargo.packages() {
        for dep in &cargo[pkg].dependencies {
            let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) else { continue };
            let Some(targets) = pkg_crates.get(&pkg) else { continue };

            let name = CrateName::new(&dep.name).unwrap();
            for &(from, kind) in targets {
                // Build scripts may only depend on build dependencies.
                if (dep.kind == DepKind::Build) != (kind == TargetKind::BuildScript) {
                    continue;
                }

                // If the dependency is a dev-dependency with both crates being member libraries of
                // the workspace we delay adding the edge. The reason can be read up on in
                // https://github.com/rust-lang/rust-analyzer/issues/14167
                // but in short, such an edge is able to cause some form of cycle in the crate graph
                // for normal dependencies. If we do run into a cycle like this, we want to prefer
                // the non dev-dependency edge, and so the easiest way to do that is by adding the
                // dev-dependency edges last.
                if dep.kind == DepKind::Dev
                    && matches!(kind, TargetKind::Lib { .. })
                    && cargo[dep.pkg].is_member
                    && cargo[pkg].is_member
                {
                    delayed_dev_deps.push((from, name.clone(), to));
                    continue;
                }

                add_dep(crate_graph, from, name.clone(), to)
            }
        }
    }

    for (from, name, to) in delayed_dev_deps {
        add_dep(crate_graph, from, name, to);
    }

    if has_private {
        // If the user provided a path to rustc sources, we add all the rustc_private crates
        // and create dependencies on them for the crates which opt-in to that
        if let Some((rustc_workspace, rustc_build_scripts)) = rustc {
            handle_rustc_crates(
                crate_graph,
                proc_macros,
                &mut pkg_to_lib_crate,
                load,
                rustc_workspace,
                cargo,
                &public_deps,
                libproc_macro,
                &pkg_crates,
                &cfg_options,
                override_cfg,
                // FIXME: Remove this once rustc switched over to rust-project.json
                if rustc_workspace.workspace_root() == cargo.workspace_root() {
                    // the rustc workspace does not use the installed toolchain's proc-macro server
                    // so we need to make sure we don't use the pre compiled proc-macros there either
                    build_scripts
                } else {
                    rustc_build_scripts
                },
            );
        }
    }
    res
}

fn detached_file_to_crate_graph(
    rustc_cfg: Vec<CfgFlag>,
    load: FileLoader<'_>,
    detached_file: &AbsPathBuf,
    sysroot: Option<&Sysroot>,
    override_cfg: &CfgOverrides,
) -> (CrateGraph, ProcMacroPaths) {
    let _p = tracing::span!(tracing::Level::INFO, "detached_files_to_crate_graph").entered();
    let mut crate_graph = CrateGraph::default();
    let (public_deps, _libproc_macro) = match sysroot {
        Some(sysroot) => sysroot_to_crate_graph(&mut crate_graph, sysroot, rustc_cfg.clone(), load),
        None => (SysrootPublicDeps::default(), None),
    };

    let mut cfg_options = CfgOptions::from_iter(rustc_cfg);
    cfg_options.insert_atom("test".into());
    cfg_options.insert_atom("rust_analyzer".into());
    override_cfg.apply(&mut cfg_options, "");
    let cfg_options = Arc::new(cfg_options);

    let file_id = match load(detached_file) {
        Some(file_id) => file_id,
        None => {
            tracing::error!("Failed to load detached file {:?}", detached_file);
            return (crate_graph, FxHashMap::default());
        }
    };
    let display_name = detached_file
        .file_stem()
        .map(|file_stem| CrateDisplayName::from_canonical_name(file_stem.to_owned()));
    let detached_file_crate = crate_graph.add_crate_root(
        file_id,
        Edition::CURRENT,
        display_name.clone(),
        None,
        cfg_options.clone(),
        None,
        Env::default(),
        false,
        CrateOrigin::Local {
            repo: None,
            name: display_name.map(|n| n.canonical_name().to_owned()),
        },
    );

    public_deps.add_to_crate_graph(&mut crate_graph, detached_file_crate);
    (crate_graph, FxHashMap::default())
}

fn handle_rustc_crates(
    crate_graph: &mut CrateGraph,
    proc_macros: &mut ProcMacroPaths,
    pkg_to_lib_crate: &mut FxHashMap<Package, CrateId>,
    load: FileLoader<'_>,
    rustc_workspace: &CargoWorkspace,
    cargo: &CargoWorkspace,
    public_deps: &SysrootPublicDeps,
    libproc_macro: Option<CrateId>,
    pkg_crates: &FxHashMap<Package, Vec<(CrateId, TargetKind)>>,
    cfg_options: &CfgOptions,
    override_cfg: &CfgOverrides,
    build_scripts: &WorkspaceBuildScripts,
) {
    let mut rustc_pkg_crates = FxHashMap::default();
    // The root package of the rustc-dev component is rustc_driver, so we match that
    let root_pkg =
        rustc_workspace.packages().find(|&package| rustc_workspace[package].name == "rustc_driver");
    // The rustc workspace might be incomplete (such as if rustc-dev is not
    // installed for the current toolchain) and `rustc_source` is set to discover.
    if let Some(root_pkg) = root_pkg {
        // Iterate through every crate in the dependency subtree of rustc_driver using BFS
        let mut queue = VecDeque::new();
        queue.push_back(root_pkg);
        while let Some(pkg) = queue.pop_front() {
            // Don't duplicate packages if they are dependent on a diamond pattern
            // N.B. if this line is omitted, we try to analyze over 4_800_000 crates
            // which is not ideal
            if rustc_pkg_crates.contains_key(&pkg) {
                continue;
            }
            for dep in &rustc_workspace[pkg].dependencies {
                queue.push_back(dep.pkg);
            }

            let mut cfg_options = cfg_options.clone();
            override_cfg.apply(&mut cfg_options, &rustc_workspace[pkg].name);

            for &tgt in rustc_workspace[pkg].targets.iter() {
                let kind @ TargetKind::Lib { is_proc_macro } = rustc_workspace[tgt].kind else {
                    continue;
                };
                let pkg_crates = &mut rustc_pkg_crates.entry(pkg).or_insert_with(Vec::new);
                if let Some(file_id) = load(&rustc_workspace[tgt].root) {
                    let crate_id = add_target_crate_root(
                        crate_graph,
                        proc_macros,
                        &rustc_workspace[pkg],
                        build_scripts.get_output(pkg),
                        cfg_options.clone(),
                        file_id,
                        &rustc_workspace[tgt].name,
                        kind,
                        CrateOrigin::Rustc { name: rustc_workspace[pkg].name.clone() },
                    );
                    pkg_to_lib_crate.insert(pkg, crate_id);
                    // Add dependencies on core / std / alloc for this crate
                    public_deps.add_to_crate_graph(crate_graph, crate_id);
                    if let Some(proc_macro) = libproc_macro {
                        add_proc_macro_dep(crate_graph, crate_id, proc_macro, is_proc_macro);
                    }
                    pkg_crates.push(crate_id);
                }
            }
        }
    }
    // Now add a dep edge from all targets of upstream to the lib
    // target of downstream.
    for pkg in rustc_pkg_crates.keys().copied() {
        for dep in rustc_workspace[pkg].dependencies.iter() {
            let name = CrateName::new(&dep.name).unwrap();
            if let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) {
                for &from in rustc_pkg_crates.get(&pkg).into_iter().flatten() {
                    add_dep(crate_graph, from, name.clone(), to);
                }
            }
        }
    }
    // Add a dependency on the rustc_private crates for all targets of each package
    // which opts in
    for dep in rustc_workspace.packages() {
        let name = CrateName::normalize_dashes(&rustc_workspace[dep].name);

        if let Some(&to) = pkg_to_lib_crate.get(&dep) {
            for pkg in cargo.packages() {
                let package = &cargo[pkg];
                if !package.metadata.rustc_private {
                    continue;
                }
                for (from, _) in pkg_crates.get(&pkg).into_iter().flatten() {
                    // Avoid creating duplicate dependencies
                    // This avoids the situation where `from` depends on e.g. `arrayvec`, but
                    // `rust_analyzer` thinks that it should use the one from the `rustc_source`
                    // instead of the one from `crates.io`
                    if !crate_graph[*from].dependencies.iter().any(|d| d.name == name) {
                        add_dep(crate_graph, *from, name.clone(), to);
                    }
                }
            }
        }
    }
}

fn add_target_crate_root(
    crate_graph: &mut CrateGraph,
    proc_macros: &mut ProcMacroPaths,
    pkg: &PackageData,
    build_data: Option<&BuildScriptOutput>,
    cfg_options: CfgOptions,
    file_id: FileId,
    cargo_name: &str,
    kind: TargetKind,
    origin: CrateOrigin,
) -> CrateId {
    let edition = pkg.edition;
    let potential_cfg_options = if pkg.features.is_empty() {
        None
    } else {
        let mut potential_cfg_options = cfg_options.clone();
        potential_cfg_options.extend(
            pkg.features
                .iter()
                .map(|feat| CfgFlag::KeyValue { key: "feature".into(), value: feat.0.into() }),
        );
        Some(potential_cfg_options)
    };
    let cfg_options = {
        let mut opts = cfg_options;
        for feature in pkg.active_features.iter() {
            opts.insert_key_value("feature".into(), feature.into());
        }
        if let Some(cfgs) = build_data.as_ref().map(|it| &it.cfgs) {
            opts.extend(cfgs.iter().cloned());
        }
        opts
    };

    let mut env = Env::default();
    inject_cargo_package_env(&mut env, pkg);
    inject_cargo_env(&mut env);
    inject_rustc_tool_env(&mut env, cargo_name, kind);

    if let Some(envs) = build_data.map(|it| &it.envs) {
        for (k, v) in envs {
            env.set(k, v.clone());
        }
    }
    let crate_id = crate_graph.add_crate_root(
        file_id,
        edition,
        Some(CrateDisplayName::from_canonical_name(cargo_name.to_owned())),
        Some(pkg.version.to_string()),
        Arc::new(cfg_options),
        potential_cfg_options.map(Arc::new),
        env,
        matches!(kind, TargetKind::Lib { is_proc_macro: true }),
        origin,
    );
    if let TargetKind::Lib { is_proc_macro: true } = kind {
        let proc_macro = match build_data.as_ref().map(|it| it.proc_macro_dylib_path.as_ref()) {
            Some(it) => it.cloned().map(|path| Ok((Some(cargo_name.to_owned()), path))),
            None => Some(Err("crate has not yet been built".to_owned())),
        };
        if let Some(proc_macro) = proc_macro {
            proc_macros.insert(crate_id, proc_macro);
        }
    }

    crate_id
}

#[derive(Default)]
struct SysrootPublicDeps {
    deps: Vec<(CrateName, CrateId, bool)>,
}

impl SysrootPublicDeps {
    /// Makes `from` depend on the public sysroot crates.
    fn add_to_crate_graph(&self, crate_graph: &mut CrateGraph, from: CrateId) {
        for (name, krate, prelude) in &self.deps {
            add_dep_with_prelude(crate_graph, from, name.clone(), *krate, *prelude);
        }
    }
}

fn sysroot_to_crate_graph(
    crate_graph: &mut CrateGraph,
    sysroot: &Sysroot,
    rustc_cfg: Vec<CfgFlag>,
    load: FileLoader<'_>,
) -> (SysrootPublicDeps, Option<CrateId>) {
    let _p = tracing::span!(tracing::Level::INFO, "sysroot_to_crate_graph").entered();
    match sysroot.mode() {
        SysrootMode::Workspace(cargo) => {
            let (mut cg, mut pm) = cargo_to_crate_graph(
                load,
                None,
                cargo,
                None,
                rustc_cfg,
                &CfgOverrides {
                    global: CfgDiff::new(
                        vec![
                            CfgAtom::Flag("debug_assertions".into()),
                            CfgAtom::Flag("miri".into()),
                        ],
                        vec![],
                    )
                    .unwrap(),
                    ..Default::default()
                },
                &WorkspaceBuildScripts::default(),
            );

            let mut pub_deps = vec![];
            let mut libproc_macro = None;
            let diff = CfgDiff::new(vec![], vec![CfgAtom::Flag("test".into())]).unwrap();
            for (cid, c) in cg.iter_mut() {
                // uninject `test` flag so `core` keeps working.
                Arc::make_mut(&mut c.cfg_options).apply_diff(diff.clone());
                // patch the origin
                if c.origin.is_local() {
                    let lang_crate = LangCrateOrigin::from(
                        c.display_name.as_ref().map_or("", |it| it.canonical_name()),
                    );
                    c.origin = CrateOrigin::Lang(lang_crate);
                    match lang_crate {
                        LangCrateOrigin::Test
                        | LangCrateOrigin::Alloc
                        | LangCrateOrigin::Core
                        | LangCrateOrigin::Std => pub_deps.push((
                            CrateName::normalize_dashes(&lang_crate.to_string()),
                            cid,
                            !matches!(lang_crate, LangCrateOrigin::Test),
                        )),
                        LangCrateOrigin::ProcMacro => libproc_macro = Some(cid),
                        LangCrateOrigin::Other => (),
                    }
                }
            }

            let mut marker_set = vec![];
            for &(_, cid, _) in pub_deps.iter() {
                marker_set.extend(cg.transitive_deps(cid));
            }
            if let Some(cid) = libproc_macro {
                marker_set.extend(cg.transitive_deps(cid));
            }

            marker_set.sort();
            marker_set.dedup();

            // Remove all crates except the ones we are interested in to keep the sysroot graph small.
            let removed_mapping = cg.remove_crates_except(&marker_set);
            let mapping = crate_graph.extend(cg, &mut pm, |(_, a), (_, b)| a == b);

            // Map the id through the removal mapping first, then through the crate graph extension mapping.
            pub_deps.iter_mut().for_each(|(_, cid, _)| {
                *cid = mapping[&removed_mapping[cid.into_raw().into_u32() as usize].unwrap()]
            });
            if let Some(libproc_macro) = &mut libproc_macro {
                *libproc_macro = mapping
                    [&removed_mapping[libproc_macro.into_raw().into_u32() as usize].unwrap()];
            }

            (SysrootPublicDeps { deps: pub_deps }, libproc_macro)
        }
        SysrootMode::Stitched(stitched) => {
            let cfg_options = Arc::new({
                let mut cfg_options = CfgOptions::default();
                cfg_options.extend(rustc_cfg);
                cfg_options.insert_atom("debug_assertions".into());
                cfg_options.insert_atom("miri".into());
                cfg_options
            });
            let sysroot_crates: FxHashMap<SysrootCrate, CrateId> = stitched
                .crates()
                .filter_map(|krate| {
                    let file_id = load(&stitched[krate].root)?;

                    let display_name =
                        CrateDisplayName::from_canonical_name(stitched[krate].name.clone());
                    let crate_id = crate_graph.add_crate_root(
                        file_id,
                        Edition::CURRENT,
                        Some(display_name),
                        None,
                        cfg_options.clone(),
                        None,
                        Env::default(),
                        false,
                        CrateOrigin::Lang(LangCrateOrigin::from(&*stitched[krate].name)),
                    );
                    Some((krate, crate_id))
                })
                .collect();

            for from in stitched.crates() {
                for &to in stitched[from].deps.iter() {
                    let name = CrateName::new(&stitched[to].name).unwrap();
                    if let (Some(&from), Some(&to)) =
                        (sysroot_crates.get(&from), sysroot_crates.get(&to))
                    {
                        add_dep(crate_graph, from, name, to);
                    }
                }
            }

            let public_deps = SysrootPublicDeps {
                deps: stitched
                    .public_deps()
                    .filter_map(|(name, idx, prelude)| {
                        Some((name, *sysroot_crates.get(&idx)?, prelude))
                    })
                    .collect::<Vec<_>>(),
            };

            let libproc_macro =
                stitched.proc_macro().and_then(|it| sysroot_crates.get(&it).copied());
            (public_deps, libproc_macro)
        }
    }
}

fn add_dep(graph: &mut CrateGraph, from: CrateId, name: CrateName, to: CrateId) {
    add_dep_inner(graph, from, Dependency::new(name, to))
}

fn add_dep_with_prelude(
    graph: &mut CrateGraph,
    from: CrateId,
    name: CrateName,
    to: CrateId,
    prelude: bool,
) {
    add_dep_inner(graph, from, Dependency::with_prelude(name, to, prelude))
}

fn add_proc_macro_dep(crate_graph: &mut CrateGraph, from: CrateId, to: CrateId, prelude: bool) {
    add_dep_with_prelude(crate_graph, from, CrateName::new("proc_macro").unwrap(), to, prelude);
}

fn add_dep_inner(graph: &mut CrateGraph, from: CrateId, dep: Dependency) {
    if let Err(err) = graph.add_dep(from, dep) {
        tracing::error!("{}", err)
    }
}
