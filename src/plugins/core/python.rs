use std::collections::HashMap;
use std::path::{Path, PathBuf};

use itertools::Itertools;
use miette::{IntoDiagnostic, Result};

use crate::cache::CacheManager;
use crate::cmd::CmdLineRunner;
use crate::config::{Config, Settings};
use crate::file::display_path;
use crate::git::Git;
use crate::http::{HTTP, HTTP_FETCH};
use crate::install_context::InstallContext;
use crate::plugins::core::CorePlugin;
use crate::plugins::Plugin;
use crate::toolset::{ToolVersion, ToolVersionRequest, Toolset};
use crate::ui::progress_report::SingleReport;
use crate::{cmd, env, file};

#[derive(Debug)]
pub struct PythonPlugin {
    core: CorePlugin,
    precompiled_cache: CacheManager<Vec<(String, String, String)>>,
}

impl PythonPlugin {
    pub fn new() -> Self {
        let core = CorePlugin::new("python");
        Self {
            precompiled_cache: CacheManager::new(core.cache_path.join("precompiled.msgpack.z"))
                .with_fresh_duration(*env::MISE_FETCH_REMOTE_VERSIONS_CACHE),
            core,
        }
    }

    fn python_build_path(&self) -> PathBuf {
        self.core.cache_path.join("pyenv")
    }
    fn python_build_bin(&self) -> PathBuf {
        self.python_build_path()
            .join("plugins/python-build/bin/python-build")
    }
    fn install_or_update_python_build(&self) -> Result<()> {
        if self.python_build_path().exists() {
            self.update_python_build()
        } else {
            self.install_python_build()
        }
    }
    fn install_python_build(&self) -> Result<()> {
        if self.python_build_path().exists() {
            return Ok(());
        }
        let python_build_path = self.python_build_path();
        debug!("Installing python-build to {}", python_build_path.display());
        file::create_dir_all(self.python_build_path().parent().unwrap())?;
        let git = Git::new(self.python_build_path());
        git.clone(&env::MISE_PYENV_REPO)?;
        Ok(())
    }
    fn update_python_build(&self) -> Result<()> {
        // TODO: do not update if recently updated
        debug!(
            "Updating python-build in {}",
            self.python_build_path().display()
        );
        let git = Git::new(self.python_build_path());
        CorePlugin::run_fetch_task_with_timeout(move || git.update(None))?;
        Ok(())
    }

    fn fetch_remote_versions(&self) -> Result<Vec<String>> {
        let settings = Settings::get();
        if self.should_install_precompiled(&settings) {
            let v = self
                .fetch_precompiled_remote_versions()?
                .iter()
                .map(|(v, _, _)| v.to_string())
                .unique()
                .collect();
            return Ok(v);
        }
        match self.core.fetch_remote_versions_from_mise() {
            Ok(Some(versions)) => return Ok(versions),
            Ok(None) => {}
            Err(e) => warn!("failed to fetch remote versions: {}", e),
        }
        self.install_or_update_python_build()?;
        let python_build_bin = self.python_build_bin();
        CorePlugin::run_fetch_task_with_timeout(move || {
            let output = cmd!(python_build_bin, "--definitions")
                .read()
                .into_diagnostic()?;
            let versions = output
                .split('\n')
                .map(|s| s.to_string())
                .sorted_by_cached_key(|v| regex!(r"^\d+").is_match(v))
                .collect();
            Ok(versions)
        })
    }

    fn python_path(&self, tv: &ToolVersion) -> PathBuf {
        tv.install_short_path().join("bin/python")
    }

    fn should_install_precompiled(&self, settings: &Settings) -> bool {
        !settings.all_compile && !settings.python_compile && settings.experimental
    }

    fn fetch_precompiled_remote_versions(&self) -> Result<&Vec<(String, String, String)>> {
        self.precompiled_cache.get_or_try_init(|| {
            let raw = HTTP_FETCH.get_text("http://mise-versions.jdx.dev/python-precompiled")?;
            let versions = raw
                .lines()
                .filter(|v| v.contains(&format!("{}-{}", arch(), os())))
                .flat_map(|v| {
                    regex!(r"^cpython-(\d+\.\d+\.\d+)\+(\d+).*")
                        .captures(v)
                        .map(|caps| {
                            (
                                caps[1].to_string(),
                                caps[2].to_string(),
                                caps[0].to_string(),
                            )
                        })
                })
                .collect_vec();
            Ok(versions)
        })
    }

    fn install_precompiled(&self, ctx: &InstallContext) -> Result<()> {
        warn!("installing precompiled python from indygreg/python-build-standalone");
        warn!("if you experience issues with this python, switch to python-build");
        warn!("by running: mise settings set python_compile 0");

        let config = Config::get();
        let precompile_info = self
            .fetch_precompiled_remote_versions()?
            .iter()
            .rev()
            .find(|(v, _, _)| &ctx.tv.version == v);
        let (tag, filename) = match precompile_info {
            Some((_, tag, filename)) => (tag, filename),
            None => bail!("no precompiled version found for {}", ctx.tv),
        };
        let url = format!(
            "https://github.com/indygreg/python-build-standalone/releases/download/{tag}/{filename}"
        );
        let filename = url.split('/').last().unwrap();
        let install = ctx.tv.install_path();
        let download = ctx.tv.download_path();
        let tarball_path = download.join(filename);

        ctx.pr.set_message(format!("downloading {}", &url));
        HTTP.download_file(&url, &tarball_path)?;

        ctx.pr
            .set_message(format!("installing {}", tarball_path.display()));
        file::untar(&tarball_path, &download)?;
        file::remove_all(&install)?;
        file::rename(download.join("python"), &install)?;
        file::make_symlink(&install.join("bin/python3"), &install.join("bin/python"))?;

        self.test_python(&config, &ctx.tv, ctx.pr.as_ref())?;

        Ok(())
    }

    fn install_default_packages(
        &self,
        config: &Config,
        tv: &ToolVersion,
        pr: &dyn SingleReport,
    ) -> Result<()> {
        if !env::MISE_PYTHON_DEFAULT_PACKAGES_FILE.exists() {
            return Ok(());
        }
        pr.set_message("installing default packages".into());
        CmdLineRunner::new(tv.install_path().join("bin/python"))
            .with_pr(pr)
            .arg("-m")
            .arg("pip")
            .arg("install")
            .arg("--upgrade")
            .arg("-r")
            .arg(&*env::MISE_PYTHON_DEFAULT_PACKAGES_FILE)
            .envs(&config.env)
            .execute()
    }

    fn get_virtualenv(
        &self,
        config: &Config,
        tv: &ToolVersion,
        pr: Option<&dyn SingleReport>,
    ) -> Result<Option<PathBuf>> {
        if let Some(virtualenv) = tv.opts.get("virtualenv") {
            let settings = Settings::try_get()?;
            if !settings.experimental {
                warn!(
                    "please enable experimental mode with `mise settings set experimental true` \
                    to use python virtualenv activation"
                );
            }
            let mut virtualenv: PathBuf = file::replace_path(Path::new(virtualenv));
            if !virtualenv.is_absolute() {
                // TODO: use the path of the config file that specified python, not the top one like this
                if let Some(project_root) = &config.project_root {
                    virtualenv = project_root.join(virtualenv);
                }
            }
            if !virtualenv.exists() {
                if settings.python_venv_auto_create {
                    info!("setting up virtualenv at: {}", virtualenv.display());
                    let mut cmd = CmdLineRunner::new(self.python_path(tv))
                        .arg("-m")
                        .arg("venv")
                        .arg(&virtualenv)
                        .envs(&config.env);
                    if let Some(pr) = pr {
                        cmd = cmd.with_pr(pr);
                    }
                    cmd.execute()?;
                } else {
                    warn!(
                        "no venv found at: {p}\n\n\
                        To have mise automatically create virtualenvs, run:\n\
                        mise settings set python_venv_auto_create true\n\n\
                        To create a virtualenv manually, run:\n\
                        python -m venv {p}",
                        p = display_path(&virtualenv)
                    );
                    return Ok(None);
                }
            }
            // TODO: enable when it is more reliable
            // self.check_venv_python(&virtualenv, tv)?;
            Ok(Some(virtualenv))
        } else {
            Ok(None)
        }
    }

    // fn check_venv_python(&self, virtualenv: &Path, tv: &ToolVersion) -> Result<()> {
    //     let symlink = virtualenv.join("bin/python");
    //     let target = self.python_path(tv);
    //     let symlink_target = symlink.read_link().unwrap_or_default();
    //     ensure!(
    //         symlink_target == target,
    //         "expected venv {} to point to {}.\nTry deleting the venv at {}.",
    //         display_path(&symlink),
    //         display_path(&target),
    //         display_path(virtualenv)
    //     );
    //     Ok(())
    // }

    fn test_python(&self, config: &Config, tv: &ToolVersion, pr: &dyn SingleReport) -> Result<()> {
        pr.set_message("python --version".into());
        CmdLineRunner::new(self.python_path(tv))
            .arg("--version")
            .envs(&config.env)
            .execute()
    }
}

impl Plugin for PythonPlugin {
    fn name(&self) -> &str {
        "python"
    }

    fn list_remote_versions(&self) -> Result<Vec<String>> {
        self.core
            .remote_version_cache
            .get_or_try_init(|| self.fetch_remote_versions())
            .cloned()
    }

    fn legacy_filenames(&self) -> Result<Vec<String>> {
        Ok(vec![".python-version".to_string()])
    }

    fn install_version_impl(&self, ctx: &InstallContext) -> Result<()> {
        let config = Config::get();
        let settings = Settings::try_get()?;
        if self.should_install_precompiled(&settings) {
            return self.install_precompiled(ctx);
        }
        self.install_or_update_python_build()?;
        if matches!(&ctx.tv.request, ToolVersionRequest::Ref(..)) {
            return Err(miette!("Ref versions not supported for python"));
        }
        ctx.pr.set_message("Running python-build".into());
        let mut cmd = CmdLineRunner::new(self.python_build_bin())
            .with_pr(ctx.pr.as_ref())
            .arg(ctx.tv.version.as_str())
            .arg(&ctx.tv.install_path())
            .envs(&config.env);
        if settings.verbose {
            cmd = cmd.arg("--verbose");
        }
        if let Some(patch_url) = &*env::MISE_PYTHON_PATCH_URL {
            ctx.pr
                .set_message(format!("with patch file from: {patch_url}"));
            let patch = HTTP.get_text(patch_url)?;
            cmd = cmd.arg("--patch").stdin_string(patch)
        }
        if let Some(patches_dir) = &*env::MISE_PYTHON_PATCHES_DIRECTORY {
            let patch_file = patches_dir.join(format!("{}.patch", &ctx.tv.version));
            if patch_file.exists() {
                ctx.pr
                    .set_message(format!("with patch file: {}", patch_file.display()));
                let contents = file::read_to_string(&patch_file)?;
                cmd = cmd.arg("--patch").stdin_string(contents);
            } else {
                warn!("patch file not found: {}", patch_file.display());
            }
        }
        cmd.execute()?;
        self.test_python(&config, &ctx.tv, ctx.pr.as_ref())?;
        if let Err(e) = self.get_virtualenv(&config, &ctx.tv, Some(ctx.pr.as_ref())) {
            warn!("failed to get virtualenv: {e}");
        }
        self.install_default_packages(&config, &ctx.tv, ctx.pr.as_ref())?;
        Ok(())
    }

    fn exec_env(
        &self,
        config: &Config,
        _ts: &Toolset,
        tv: &ToolVersion,
    ) -> Result<HashMap<String, String>> {
        let mut hm = HashMap::new();
        match self.get_virtualenv(config, tv, None) {
            Err(e) => warn!("failed to get virtualenv: {e}"),
            Ok(Some(virtualenv)) => {
                let bin = virtualenv.join("bin");
                hm.insert("VIRTUAL_ENV".into(), virtualenv.to_string_lossy().into());
                hm.insert("MISE_ADD_PATH".into(), bin.to_string_lossy().into());
            }
            Ok(None) => {}
        };
        Ok(hm)
    }
}

fn os() -> &'static str {
    if cfg!(target_env = "musl") {
        "unknown-linux-musl"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-gnu"
    } else if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        panic!("unsupported OS")
    }
}

fn arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64_v3" // TODO: make the version configurable
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        panic!("unsupported arch")
    }
}
