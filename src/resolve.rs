use crate::actions::{
    Actions, AurPackage, AurUpdate, AurUpdates, Missing, RepoPackage, Unneeded, Want,
};
use crate::base::Base;
use crate::repo::Repo;
use crate::satisfies::{satisfies_provide, Satisfies};
use crate::{AurBase, CustomPackage, CustomPackages, Error};
use bitflags::bitflags;

use std::collections::HashSet;
use std::fmt;

use alpm::{Alpm, Db, Dep, Depend, Version};
use alpm_utils::{AsTarg, DbListExt, Targ};
use log::Level::Debug;
use log::{debug, log_enabled};
use raur::{ArcPackage, Cache, Raur, SearchBy};

// TODO: custom repo will not bundle pkg specific deps, which means a package from a srcinfo
// already in build may not actually already be fully satisfied. check for this and if not push it
// as a new pkgbase

bitflags! {
    /// Config options for Handle.
    pub struct Flags: u32 {
        /// Do not resolve dependencies.
        const NO_DEPS = 1 << 2;
        /// Do not enforse version constraints on dependencies.
        const NO_DEP_VERSION = 1 << 3;
        /// Solve provides for targets.
        const TARGET_PROVIDES = 1 << 4;
        /// Solve provides for missing packages.
        const MISSING_PROVIDES = 1 << 5;
        /// Solve provides in all other instances.
        const PROVIDES = 1 << 6;
        /// Calculate which packages are only needed to build the packages.
        const CALCULATE_MAKE = 1 << 7;
        /// Solve checkdepends.
        const CHECK_DEPENDS = 1 << 9;
        /// Ignore targets that are up to date.
        const NEEDED = 1 << 10;
        /// Search aur for targets.
        const AUR = 1 << 11;
        /// Search alpm repos for targets.
        const NATIVE_REPO = 1 << 12;
        /// when fetching updates, also include packages that are older than locally installed.
        const ENABLE_DOWNGRADE = 1 << 13;
        /// Asume packages are going to be put in a local repo.
        ///
        /// This means that we need to still build packages even if they are already installed.
        const LOCAL_REPO = 1 << 14;
    }
}

impl Flags {
    /// Create a new Flags with the default configuration
    pub fn new() -> Self {
        Flags::CALCULATE_MAKE
            | Flags::TARGET_PROVIDES
            | Flags::MISSING_PROVIDES
            | Flags::CHECK_DEPENDS
            | Flags::AUR
            | Flags::NATIVE_REPO
    }

    /// Create a new Flags with repo targets disabled
    pub fn aur_only() -> Self {
        Flags::new() & !Flags::NATIVE_REPO
    }
}

impl Default for Flags {
    fn default() -> Self {
        Self::new()
    }
}

struct ProviderCallback(Box<dyn Fn(&str, &[&str]) -> usize>);
struct GroupCallback<'a>(Box<dyn Fn(&[Group<'a>]) -> Vec<alpm::Package<'a>>>);
struct IsDevel(Box<dyn Fn(&str) -> bool>);

impl fmt::Debug for ProviderCallback {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("ProviderCallback")
    }
}

impl<'a> fmt::Debug for GroupCallback<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("GroupCallback")
    }
}

impl<'a> GroupCallback<'a> {
    fn new<F: Fn(&[Group<'a>]) -> Vec<alpm::Package<'a>> + 'static>(f: F) -> Self {
        GroupCallback(Box::new(f))
    }
}

impl ProviderCallback {
    fn new<F: Fn(&str, &[&str]) -> usize + 'static>(f: F) -> Self {
        ProviderCallback(Box::new(f))
    }
}

impl fmt::Debug for IsDevel {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("IsDevel")
    }
}

impl IsDevel {
    fn new<F: Fn(&str) -> bool + 'static>(f: F) -> Self {
        IsDevel(Box::new(f))
    }
}

/// An alpm Db+Group pair passed to the group callback.
pub struct Group<'a> {
    /// The db the group belongs to.
    pub db: Db<'a>,
    /// The group.
    pub group: alpm::Group<'a>,
}

#[derive(Debug)]
enum AurOrCustomPackage<'a> {
    Aur(&'a raur::Package),
    Custom(&'a str, &'a srcinfo::Srcinfo, &'a srcinfo::Package),
}

impl<'a> AurOrCustomPackage<'a> {
    fn pkgbase(&self) -> &str {
        match self {
            AurOrCustomPackage::Aur(pkg) => &pkg.package_base,
            AurOrCustomPackage::Custom(_, base, _) => &base.base.pkgbase,
        }
    }

    fn depends(&self, arch: &str, check_depends: bool) -> Vec<&str> {
        match self {
            AurOrCustomPackage::Aur(pkg) => {
                let check = if check_depends {
                    Some(&pkg.check_depends)
                } else {
                    None
                };

                pkg.make_depends
                    .iter()
                    .chain(check.into_iter().flatten())
                    .chain(&pkg.depends)
                    .map(|s| s.as_str())
                    .collect()
            }
            AurOrCustomPackage::Custom(_, base, pkg) => {
                let base = &base.base;
                let check = if check_depends {
                    Some(&base.checkdepends)
                } else {
                    None
                };

                base.makedepends
                    .iter()
                    .chain(check.into_iter().flatten())
                    .chain(&pkg.depends)
                    .filter(|d| d.arch == None || d.arch.as_deref() == Some(arch))
                    .flat_map(|d| &d.vec)
                    .map(|d| d.as_str())
                    .collect()
            }
        }
    }
}

/// Resolver is the main type for resolving dependencies
///
/// Given a list of targets of either repo or AUR packages it will resolve the dependencies needed
/// to install them.
///
/// This resolver assumes all the repo packages will be installed first, then each base is built
/// and installed together.
///
/// aur-depends will try to solve dependnecies using the minimum ammount of AUR RPC requests.
///
/// Resolving is done via the AUR RPC. No packages are downloaded.
///
/// # Example
///
/// ```no_run
/// # use aur_depends::Error;
/// # #[tokio::test]
/// # async fn run() -> Result<(), Error> {
/// use std::collections::HashSet;
/// use alpm::Alpm;
/// use raur::Handle;
///
/// use aur_depends::{Flags, Resolver};
///
/// let alpm = Alpm::new("/", "/var/lib/pacman")?;
/// let raur = Handle::default();
/// let mut cache = HashSet::new();
/// let resolver = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::aur());
/// let actions = resolver.resolve_targets(&["discord-canary", "spotify"]).await?;
///
/// for install in &actions.install {
///     println!("install: {}", install.pkg.name())
/// }
///
/// for build in actions.iter_build_pkgs() {
///     println!("build: {}", build.pkg.name)
/// }
///
/// # Ok (())
/// # }
/// ```
#[derive(Debug)]
pub struct Resolver<'a, 'b, H = raur::Handle> {
    alpm: &'a Alpm,
    repos: Vec<Repo>,
    resolved: HashSet<String>,
    cache: &'b mut Cache,
    stack: Vec<Want>,
    raur: &'b H,
    actions: Actions<'a>,
    seen: HashSet<String>,
    flags: Flags,
    provider_callback: Option<ProviderCallback>,
    group_callback: Option<GroupCallback<'a>>,
    is_devel: Option<IsDevel>,
    aur_namespace: Option<String>,
}

impl<'a, 'b, E: std::error::Error + Sync + Send + 'static, H: Raur<Err = E> + Sync>
    Resolver<'a, 'b, H>
{
    /// Create a new Resolver
    pub fn new(alpm: &'a Alpm, cache: &'b mut Cache, raur: &'b H, flags: Flags) -> Self {
        let actions = Actions {
            alpm,
            missing: Vec::new(),
            unneeded: Vec::new(),
            build: Vec::new(),
            install: Vec::new(),
        };

        Resolver {
            alpm,
            repos: Vec::new(),
            resolved: HashSet::new(),
            cache,
            stack: Vec::new(),
            actions,
            raur,
            flags,
            seen: HashSet::new(),
            provider_callback: None,
            group_callback: None,
            is_devel: None,
            aur_namespace: None,
        }
    }

    /// Set the provider callback
    ///
    /// The provider callback will be called any time there is a choice of multiple AUR packages
    /// that can satisfy a dependency. This callback receives the dependency that we are trying to
    /// satisfy and a slice of package names satisfying it.
    ///
    /// The callback returns returns the index of which package to pick.
    ///
    /// Retuning an invalid index will cause a panic.
    pub fn provider_callback<F: Fn(&str, &[&str]) -> usize + 'static>(mut self, f: F) -> Self {
        self.provider_callback = Some(ProviderCallback::new(f));
        self
    }

    /// Set the group callback
    ///
    /// The group callback is called whenever a group is processed. The callback recieves the group
    /// and returns a list of packages should be installed from the group;
    ///
    pub fn group_callback<F: Fn(&[Group<'a>]) -> Vec<alpm::Package<'a>> + 'static>(
        mut self,
        f: F,
    ) -> Self {
        self.group_callback = Some(GroupCallback::new(f));
        self
    }

    /// Set the function for determining if a package is devel.
    ///
    /// Devel packages are never skipped when using NEEDED.
    ///
    /// By default, no packages are considered devel.
    pub fn devel_pkgs<F: Fn(&str) -> bool + 'static>(mut self, f: F) -> Self {
        self.is_devel = Some(IsDevel::new(f));
        self
    }

    /// If enabled, causes `aur/foo` to mean from the AUR, instead of a repo named `aur`.
    pub fn aur_namespace(mut self, enable: bool) -> Self {
        if enable {
            self.aur_namespace = Some("aur".to_string());
        }
        self
    }

    /// Causes `<name>/foo` to mean from the AUR, instead of a repo named `<name>`.
    pub fn custom_aur_namespace(mut self, name: Option<String>) -> Self {
        self.aur_namespace = name;
        self
    }

    /// Set the custom repos to use.
    pub fn repos(mut self, repos: Vec<Repo>) -> Self {
        self.repos = repos;
        self
    }

    /// Getter for the aur cache
    pub fn get_cache(&self) -> &Cache {
        self.cache
    }

    /// Mut getter for the aur cache
    pub fn get_cache_mut(&mut self) -> &mut Cache {
        self.cache
    }

    /// Get which aur packages need to be updated.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use aur_depends::{Error, AurUpdates};
    /// # #[tokio::test]
    /// # async fn run() -> Result<(), Error> {
    /// use std::collections::HashSet;
    /// use alpm::Alpm;
    /// use raur::Handle;
    ///
    /// use aur_depends::{Flags, Resolver};
    ///
    /// let alpm = Alpm::new("/", "/var/lib/pacman")?;
    /// let raur = Handle::default();
    /// let mut cache = HashSet::new();
    /// let mut resolver = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::aur_only());
    ///
    /// let updates = resolver.aur_updates().await?;
    ///
    /// for update in updates.updates {
    ///     println!("update: {}: {} -> {}", update.local.name(), update.local.version(),
    ///     update.remote.version);
    /// }
    /// # Ok (())
    /// # }
    /// ```
    pub async fn aur_updates(&mut self) -> Result<AurUpdates<'a>, Error> {
        let local_pkgs = self
            .alpm
            .localdb()
            .pkgs()
            .iter()
            .filter(|p| self.alpm.syncdbs().pkg(p.name()).is_err())
            .collect::<Vec<_>>();

        let local_pkg_names = local_pkgs.iter().map(|pkg| pkg.name()).collect::<Vec<_>>();
        self.raur
            .cache_info(self.cache, &local_pkg_names)
            .await
            .map_err(|e| Error::Raur(Box::new(e)))?;
        let mut missing = Vec::new();
        let mut ignored = Vec::new();

        let to_upgrade = local_pkgs
            .into_iter()
            .filter_map(|local_pkg| {
                if let Some(pkg) = self.cache.get(local_pkg.name()) {
                    let should_upgrade = if self.flags.contains(Flags::ENABLE_DOWNGRADE) {
                        Version::new(&*pkg.version) != local_pkg.version()
                    } else {
                        Version::new(&*pkg.version) > local_pkg.version()
                    };

                    if should_upgrade {
                        let should_ignore = local_pkg.should_ignore();

                        let up = AurUpdate {
                            local: local_pkg,
                            remote: pkg.clone(),
                        };
                        if should_ignore {
                            ignored.push(up);
                            return None;
                        }

                        return Some(up);
                    }
                } else {
                    missing.push(local_pkg);
                }

                None
            })
            .collect::<Vec<_>>();

        let updates = AurUpdates {
            updates: to_upgrade,
            missing,
            ignored,
        };
        Ok(updates)
    }

    /// Fetch updates from a list of local repos.
    pub async fn local_aur_updates<S: AsRef<str>>(
        &mut self,
        repos: &[S],
    ) -> Result<AurUpdates<'a>, Error> {
        let mut dbs = self.alpm.syncdbs().to_list_mut();
        dbs.retain(|db| repos.iter().any(|repo| repo.as_ref() == db.name()));

        let all_pkgs = dbs
            .iter()
            .flat_map(|db| db.pkgs())
            .map(|p| p.name())
            .collect::<Vec<_>>();
        self.raur
            .cache_info(self.cache, &all_pkgs)
            .await
            .map_err(|e| Error::Raur(Box::new(e)))?;

        let mut updates = Vec::new();
        let mut seen = HashSet::new();
        let mut missing = Vec::new();
        let mut ignored = Vec::new();

        for db in dbs {
            let local_pkgs = db.pkgs();
            let to_upgrade = local_pkgs
                .into_iter()
                .filter_map(|local_pkg| {
                    if let Some(pkg) = self.cache.get(local_pkg.name()) {
                        let should_upgrade = if self.flags.contains(Flags::ENABLE_DOWNGRADE) {
                            Version::new(&*pkg.version) != local_pkg.version()
                        } else {
                            Version::new(&*pkg.version) > local_pkg.version()
                        };

                        if should_upgrade {
                            let should_ignore = local_pkg.should_ignore();

                            let up = AurUpdate {
                                local: local_pkg,
                                remote: pkg.clone(),
                            };
                            if should_ignore {
                                ignored.push(up);
                                return None;
                            }

                            return Some(up);
                        }
                    } else {
                        missing.push(local_pkg);
                    }

                    None
                })
                .filter(|p| !seen.contains(p.local.name()))
                .collect::<Vec<_>>();

            seen.extend(to_upgrade.iter().map(|p| p.local.name()));
            updates.extend(to_upgrade);
        }

        let updates = AurUpdates {
            updates,
            missing,
            ignored,
        };

        Ok(updates)
    }

    /// Resolve a list of targets.
    pub async fn resolve_targets<T: AsTarg>(self, pkgs: &[T]) -> Result<Actions<'a>, Error> {
        self.resolve(pkgs, &[], true).await
    }

    /// Resolve a list of dependencies.
    pub async fn resolve_depends<T: AsRef<str>>(
        self,
        deps: &[T],
        make_deps: &[T],
    ) -> Result<Actions<'a>, Error> {
        self.resolve(deps, make_deps, false).await
    }

    async fn resolve<T: AsTarg>(
        mut self,
        deps: &[T],
        make_deps: &[T],
        is_target: bool,
    ) -> Result<Actions<'a>, Error> {
        let mut aur_targets = Vec::new();
        let mut repo_targets = Vec::new();
        let mut custom_repo_targets = Vec::new();

        let make = make_deps
            .iter()
            .map(|t| t.as_targ().pkg)
            .collect::<HashSet<&str>>();
        let localdb = self.alpm.localdb();

        'deps: for pkg in deps.iter().chain(make_deps) {
            let pkg = pkg.as_targ();
            // TODO
            // Not handle repo/pkg for !is_target

            let dep = Depend::new(pkg.to_string());
            if !is_target && self.assume_installed(&dep) {
                continue;
            }

            if self.aur_namespace.is_some() && pkg.repo == self.aur_namespace.as_deref() {
                aur_targets.push(pkg.pkg);
                continue;
            }

            if self.flags.contains(Flags::NATIVE_REPO) || !is_target {
                if let Some(alpm_pkg) = self.find_repo_satisfier(pkg.pkg) {
                    repo_targets.push((pkg, alpm_pkg));
                    continue;
                }

                if is_target {
                    let groups = self
                        .alpm
                        .syncdbs()
                        .iter()
                        .filter(|db| pkg.repo.is_none() || pkg.repo.unwrap() == db.name())
                        .filter_map(|db| db.group(pkg.pkg).map(|group| Group { db, group }).ok())
                        .collect::<Vec<_>>();
                    if !groups.is_empty() {
                        if let Some(ref f) = self.group_callback {
                            for alpm_pkg in f.0(&groups) {
                                repo_targets.push((pkg, alpm_pkg));
                            }
                        } else {
                            for group in groups {
                                for alpm_pkg in group.group.packages() {
                                    repo_targets.push((pkg, alpm_pkg));
                                }
                            }
                        }
                        continue;
                    }
                }
            }

            for repo in &self.repos {
                if pkg.repo.is_some() && pkg.repo != Some(repo.name.as_str()) {
                    continue;
                }

                for base in &repo.pkgs {
                    if base.satisfies_dep(
                        &Depend::new(pkg.pkg),
                        self.flags.contains(Flags::NO_DEP_VERSION),
                    ) {
                        custom_repo_targets.push(pkg);
                        continue 'deps;
                    }
                }
            }

            if pkg.repo.is_none() && (self.flags.contains(Flags::AUR) || !is_target) {
                aur_targets.push(pkg.pkg);
                continue;
            }

            self.actions.missing.push(Missing {
                dep: pkg.to_string(),
                stack: Vec::new(),
            });
        }

        debug!("aur targets are {:?}", aur_targets);
        debug!("custom targets are {:?}", custom_repo_targets);

        self.cache_aur_pkgs_recursive(&aur_targets, &custom_repo_targets, true)
            .await?;
        self.resolved.clear();

        for (pkg, alpm_pkg) in repo_targets {
            if !is_target && localdb.pkgs().find_satisfier(pkg.pkg).is_some() {
                continue;
            }

            if self.flags.contains(Flags::NEEDED) {
                if let Ok(local) = localdb.pkg(alpm_pkg.name()) {
                    if local.version() >= alpm_pkg.version() {
                        let unneeded = Unneeded::new(pkg.to_string(), local.version().to_string());
                        self.actions.unneeded.push(unneeded);
                        continue;
                    }
                }
            }

            let is_make = make.contains(&pkg.pkg);
            self.resolve_repo_pkg(alpm_pkg, is_target, is_make)?;
        }

        debug!("Caching done, building tree");
        debug!("targets: {:?}", aur_targets);

        for &pkg in &custom_repo_targets {
            self.resolve_custom_target(pkg, &make, is_target, &aur_targets)?;
        }

        for &aur_pkg in &aur_targets {
            self.resolve_aur_target(aur_pkg, &make, is_target, &aur_targets)?;
        }

        if self.flags.contains(Flags::CALCULATE_MAKE) {
            self.calculate_make();
        }

        Ok(self.actions)
    }

    fn resolve_aur_target(
        &mut self,
        aur_pkg: &str,
        make: &HashSet<&str>,
        is_target: bool,
        targs: &[&str],
    ) -> Result<(), Error> {
        let dep = Depend::new(aur_pkg);
        let localdb = self.alpm.localdb();

        if self.should_skip_aur_pkg(&dep, is_target) {
            return Ok(());
        }

        let pkg = if let Some(pkg) = self.select_satisfier_aur_cache(&dep, is_target) {
            pkg.clone()
        } else {
            self.actions.missing.push(Missing {
                dep: dep.to_string(),
                stack: self.stack.clone(),
            });
            return Ok(());
        };

        if self.flags.contains(Flags::NEEDED) || !is_target {
            let is_devel = self
                .is_devel
                .as_ref()
                .map(|f| f.0(aur_pkg))
                .unwrap_or(false);

            if !is_devel {
                if let Ok(local) = localdb.pkg(&*pkg.name) {
                    if local.version() >= Version::new(&*pkg.version) {
                        let unneeded =
                            Unneeded::new(aur_pkg.to_string(), local.version().to_string());
                        self.actions.unneeded.push(unneeded);
                        return Ok(());
                    }
                }
            }
        }

        let is_make = make.contains(&aur_pkg);
        self.stack
            .push(new_want(pkg.name.to_string(), aur_pkg.to_string()));
        self.resolve_aur_pkg_deps(targs, AurOrCustomPackage::Aur(&*pkg), is_make)?;
        self.stack.pop().unwrap();

        if self.actions.iter_aur_pkgs().any(|p| p.pkg.name == pkg.name) {
            return Ok(());
        }
        if self
            .actions
            .iter_custom_pkgs()
            .any(|p| p.1.pkg.pkgname == pkg.name)
        {
            return Ok(());
        }

        let p = AurPackage {
            pkg: pkg.clone(),
            make: false,
            target: is_target,
        };

        self.push_aur_build(&pkg.package_base, p);
        Ok(())
    }

    fn resolve_custom_target(
        &mut self,
        custom_pkg: Targ,
        make: &HashSet<&str>,
        is_target: bool,
        targs: &[&str],
    ) -> Result<(), Error> {
        let dep = Depend::new(custom_pkg.pkg);
        let localdb = self.alpm.localdb();

        if self.should_skip_aur_pkg(&dep, is_target) {
            return Ok(());
        }

        let (repo, base, pkg) =
            if let Some((repo, base, pkg)) = self.find_custom_repo_dep(custom_pkg.repo, &dep) {
                (repo, base, pkg)
            } else {
                self.actions.missing.push(Missing {
                    dep: dep.to_string(),
                    stack: self.stack.clone(),
                });
                return Ok(());
            };

        if self.flags.contains(Flags::NEEDED) || !is_target {
            let is_devel = self
                .is_devel
                .as_ref()
                .map(|f| f.0(custom_pkg.pkg))
                .unwrap_or(false);

            if !is_devel {
                if let Ok(local) = localdb.pkg(&*pkg.pkgname) {
                    if local.version() >= Version::new(base.version()) {
                        let unneeded =
                            Unneeded::new(custom_pkg.to_string(), local.version().to_string());
                        self.actions.unneeded.push(unneeded);
                        return Ok(());
                    }
                }
            }
        }

        let base = base.clone();
        let pkg = pkg.clone();
        let repo = repo.to_string();
        let is_make = make.contains(&custom_pkg.pkg);
        self.stack.push(new_want(
            pkg.pkgname.to_string(),
            custom_pkg.pkg.to_string(),
        ));
        self.resolve_aur_pkg_deps(
            targs,
            AurOrCustomPackage::Custom(&repo, &base, &pkg),
            is_make,
        )?;
        self.stack.pop().unwrap();

        if self
            .actions
            .iter_aur_pkgs()
            .any(|p| p.pkg.name == pkg.pkgname)
        {
            return Ok(());
        }
        if self
            .actions
            .iter_custom_pkgs()
            .any(|p| p.1.pkg.pkgname == pkg.pkgname)
        {
            return Ok(());
        }

        let p = CustomPackage {
            pkg: pkg.clone(),
            make: false,
            target: is_target,
        };

        self.push_custom_build(repo.to_string(), base, p);
        Ok(())
    }

    fn find_satisfier_aur_cache(&self, dep: &Dep) -> Option<&ArcPackage> {
        if let Some(pkg) = self.cache.get(dep.name()) {
            if pkg.satisfies_dep(dep, self.flags.contains(Flags::NO_DEP_VERSION)) {
                return Some(pkg);
            }
        }

        self.cache
            .iter()
            .find(|pkg| pkg.satisfies_dep(dep, self.flags.contains(Flags::NO_DEP_VERSION)))
    }

    /// Expected behaviour
    /// pull in a list of all matches, if one is installed, default to it.
    /// unless we are looking for a target, then always show all options.
    fn select_satisfier_aur_cache(&self, dep: &Dep, target: bool) -> Option<&ArcPackage> {
        debug!("select satisfier: {}", dep);
        if let Some(ref f) = self.provider_callback {
            let mut pkgs = self
                .cache
                .iter()
                .filter(|pkg| pkg.satisfies_dep(dep, self.flags.contains(Flags::NO_DEP_VERSION)))
                .map(|pkg| pkg.name.as_str())
                .collect::<Vec<_>>();

            debug!("satisfiers for '{:?}': {:?})", dep.to_string(), pkgs);

            if !target {
                if let Some(pkg) = pkgs.iter().find(|&&p| p == dep.name()) {
                    debug!("picked from cache: {}", pkg);
                    return self.cache.get(*pkg);
                }
            }

            if pkgs.len() == 1 {
                return self.cache.get(pkgs[0]);
            } else if pkgs.is_empty() {
                return None;
            }

            if !target {
                for &pkg in &pkgs {
                    if self.alpm.localdb().pkg(pkg).is_ok() {
                        return self.cache.get(pkg);
                    }
                }
            }

            if let Some(true_pkg) = pkgs.iter().position(|pkg| *pkg == dep.name()) {
                pkgs.swap(true_pkg, 0);
                pkgs[1..].sort_unstable();
            } else {
                pkgs.sort_unstable();
            }

            let choice = f.0(dep.to_string().as_str(), &pkgs);
            debug!("choice was: {}={}", choice, pkgs[choice]);
            self.cache.get(pkgs[choice])
        } else {
            debug!("no provider callback");
            self.find_satisfier_aur_cache(dep)
        }
    }

    fn resolve_aur_pkg_deps(
        &mut self,
        targs: &[&str],
        pkg: AurOrCustomPackage,
        make: bool,
    ) -> Result<(), Error> {
        debug!("resolve custom repo pkg deps: {}", pkg.pkgbase());
        if !self.flags.contains(Flags::NO_DEPS) {
            for dep_str in pkg.depends(
                self.alpm.architectures().first().unwrap_or(""),
                self.flags.contains(Flags::CHECK_DEPENDS),
            ) {
                debug!("depend: {}", dep_str);
                let dep = Depend::new(dep_str.to_string());

                if self.satisfied_build(&dep) || self.resolved.contains(&dep.to_string()) {
                    continue;
                }

                let is_aur_targ = self.dep_is_aur_targ(targs, &dep);

                if self.should_skip_aur_pkg(&dep, is_aur_targ) {
                    continue;
                }
                if self.satisfied_install(&dep) {
                    continue;
                }

                self.resolved.insert(dep.to_string());

                if !is_aur_targ {
                    let dep = dep.to_string();
                    if let Some(pkg) = self.find_repo_satisfier(&dep) {
                        self.stack.push(new_want(pkg.name().to_string(), dep));
                        self.resolve_repo_pkg(pkg, false, true)?;
                        self.stack.pop().unwrap();
                        continue;
                    }
                }

                if let Some((repo, base, pkg)) = self.find_custom_repo_dep(None, &dep) {
                    let repo = repo.to_string();
                    let base = base.clone();
                    let pkg = pkg.clone();
                    self.stack
                        .push(new_want(pkg.pkgname.to_string(), dep.to_string()));
                    self.resolve_aur_pkg_deps(
                        targs,
                        AurOrCustomPackage::Custom(&repo, &base, &pkg),
                        true,
                    )?;
                    self.stack.pop();

                    let pkg = CustomPackage {
                        pkg,
                        make,
                        target: false,
                    };

                    self.push_custom_build(repo.to_string(), base, pkg);
                    continue;
                }

                let sat_pkg = if let Some(pkg) = self.select_satisfier_aur_cache(&dep, false) {
                    pkg.clone()
                } else {
                    debug!(
                        "failed to find '{}' in custom repo or aur cache",
                        dep.to_string(),
                    );
                    if log_enabled!(Debug) {
                        debug!(
                            "at time of failure pkgcache is: {:?}\n",
                            self.cache.iter().map(|p| &p.name).collect::<Vec<_>>()
                        );
                        debug!("stack is: {:?}", self.stack);
                    }

                    self.actions.missing.push(Missing {
                        dep: dep.to_string(),
                        stack: self.stack.clone(),
                    });
                    continue;
                };

                self.stack
                    .push(new_want(sat_pkg.name.to_string(), dep.to_string()));
                self.resolve_aur_pkg_deps(targs, AurOrCustomPackage::Aur(&*sat_pkg), true)?;
                self.stack.pop();

                let p = AurPackage {
                    pkg: sat_pkg.clone(),
                    make,
                    target: false,
                };

                self.push_aur_build(&sat_pkg.package_base, p);
            }
        }

        Ok(())
    }

    fn find_custom_repo_dep(
        &self,
        repo_targ: Option<&str>,
        dep: &Depend,
    ) -> Option<(&str, &srcinfo::Srcinfo, &srcinfo::Package)> {
        for repo in &self.repos {
            if repo_targ.is_some() && Some(repo.name.as_str()) != repo_targ {
                continue;
            }

            for base in &repo.pkgs {
                if let Some(pkg) =
                    base.which_satisfies_dep(dep, self.flags.contains(Flags::NO_DEP_VERSION))
                {
                    return Some((&repo.name, base, base.pkg(pkg).unwrap()));
                }
            }
        }
        None
    }

    fn should_skip_aur_pkg(&self, dep: &Depend, is_target: bool) -> bool {
        if !is_target {
            if !self.flags.contains(Flags::LOCAL_REPO) && self.satisfied_local(dep) {
                return true;
            }
            if self.flags.contains(Flags::LOCAL_REPO)
                && self.find_repo_satisfier_silent(dep.to_string()).is_some()
                && self.satisfied_local(dep)
            {
                return true;
            }
            if self.assume_installed(dep) {
                return true;
            }
        }

        false
    }

    fn resolve_repo_pkg(
        &mut self,
        pkg: alpm::Package<'a>,
        target: bool,
        make: bool,
    ) -> Result<(), Error> {
        if !self.seen.insert(pkg.name().to_string()) {
            return Ok(());
        }

        if !self.flags.contains(Flags::NO_DEPS) {
            for dep in pkg.depends() {
                if self.satisfied_install(&dep)
                    || self.satisfied_local(&dep)
                    || self.assume_installed(&dep)
                {
                    continue;
                }

                if let Some(pkg) = self.find_repo_satisfier(dep.to_string()) {
                    self.resolve_repo_pkg(pkg, false, true)?;
                } else {
                    self.actions.missing.push(Missing {
                        dep: dep.to_string(),
                        stack: Vec::new(),
                    });
                }
            }
        }

        debug!("pushing to install: {}", pkg.name());
        self.actions.install.push(RepoPackage { pkg, make, target });

        Ok(())
    }

    async fn cache_aur_pkgs<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
        target: bool,
    ) -> Result<Vec<ArcPackage>, Error> {
        let mut pkgs_nover = pkgs
            .iter()
            .map(|p| p.as_ref().split(is_ver_char).next().unwrap())
            .collect::<Vec<_>>();
        pkgs_nover.sort_unstable();
        pkgs_nover.dedup();

        if (!target && self.flags.contains(Flags::PROVIDES))
            || (target && self.flags.contains(Flags::TARGET_PROVIDES))
        {
            self.cache_provides(&pkgs_nover).await
        } else {
            let mut info = self
                .raur
                .cache_info(self.cache, &pkgs_nover)
                .await
                .map_err(|e| Error::Raur(Box::new(e)))?;

            if self.flags.contains(Flags::MISSING_PROVIDES) {
                let missing = pkgs
                    .iter()
                    .map(|pkg| Depend::new(pkg.as_ref()))
                    .filter(|dep| {
                        !info.iter().any(|info| {
                            info.satisfies_dep(dep, self.flags.contains(Flags::NO_DEP_VERSION))
                        })
                    })
                    .map(|dep| dep.to_string())
                    .collect::<Vec<_>>();

                if !missing.is_empty() {
                    debug!("attempting to find provides for missing: {:?}", missing);
                    info.extend(self.cache_provides(&missing).await?);
                }
            }

            if log_enabled!(Debug) {
                debug!(
                    "provides resolved {:?} found {:?}",
                    pkgs.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
                    info.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
                );
            }
            Ok(info)
        }
    }

    async fn cache_provides<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
    ) -> Result<Vec<ArcPackage>, Error> {
        let mut to_info = pkgs
            .iter()
            .map(|s| s.as_ref().to_string())
            .collect::<Vec<_>>();

        debug!("cache args: {:?}\n", to_info);
        for pkg in pkgs {
            let pkg = pkg.as_ref();

            // Optimization, may break with alpm repos disabled
            // for example, trying to resolve "pacman" with aur only should pull in
            // "pacman-git". But because pacman is installed locally, this optimization
            // causes us to not cache "pacman-git" and end up with missing.
            //
            // TODO: maybe check for local && not sync
            if self.alpm.localdb().pkg(pkg).is_ok() {
                continue;
            }

            if let Some(word) = pkg.rsplitn(2, split_pkgname).last() {
                debug!("provide search: {} {}", pkg, word);
                to_info.extend(
                    self.raur
                        .search_by(word, SearchBy::NameDesc)
                        .await
                        .unwrap_or_else(|e| {
                            debug!("provide search '{}' failed: {}", word, e);
                            Vec::new()
                        })
                        .into_iter()
                        .map(|p| p.name),
                );
            }
        }

        to_info.sort();
        to_info.dedup();

        debug!("trying to cache {:?}\n", to_info);

        let mut ret = self
            .raur
            .cache_info(self.cache, &to_info)
            .await
            .map_err(|e| Error::Raur(Box::new(e)))?;

        ret.retain(|pkg| {
            pkgs.iter().any(|dep| {
                pkg.satisfies_dep(
                    &Depend::new(dep.as_ref()),
                    self.flags.contains(Flags::NO_DEP_VERSION),
                )
            })
        });

        Ok(ret)
    }

    async fn cache_aur_pkgs_recursive<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
        custom_pkgs: &[Targ<'_>],
        target: bool,
    ) -> Result<(), Error> {
        let mut new_pkgs = self
            .cache_aur_pkgs_recursive2(pkgs, custom_pkgs, target)
            .await?;

        while !new_pkgs.is_empty() {
            let (custom_pkgs, pkgs): (Vec<_>, Vec<_>) = new_pkgs.into_iter().partition(|p| {
                self.find_custom_repo_dep(None, &Depend::new(p.as_str()))
                    .is_some()
            });
            let custom_pkgs = custom_pkgs
                .iter()
                .map(|p| Targ {
                    repo: None,
                    pkg: p.as_str(),
                })
                .collect::<Vec<_>>();

            new_pkgs = self
                .cache_aur_pkgs_recursive2(&pkgs, &custom_pkgs, false)
                .await?;
        }

        Ok(())
    }

    fn find_aur_deps_of_custom(&mut self, targ: Targ<'_>) -> Vec<String> {
        let mut ret = Vec::new();
        if self.flags.contains(Flags::NO_DEPS) {
            return Vec::new();
        }
        let mut new_resolved = HashSet::new();

        let pkg = Depend::new(targ.pkg);
        if let Some((repo, base, pkg)) = self.find_custom_repo_dep(targ.repo, &pkg) {
            let arch = self.alpm.architectures().first().unwrap_or("");
            let custom = AurOrCustomPackage::Custom(repo, base, pkg);
            let deps = custom.depends(arch, self.flags.contains(Flags::NO_DEP_VERSION));

            for pkg in deps {
                let dep = Depend::new(pkg);
                if (!self.flags.contains(Flags::LOCAL_REPO)
                    && self.find_repo_satisfier_silent(dep.to_string()).is_some()
                    && self.satisfied_local(&dep))
                    || self.find_repo_satisfier_silent(&pkg).is_some()
                    || self.resolved.contains(&dep.to_string())
                    || self.assume_installed(&dep)
                {
                    continue;
                }

                new_resolved.insert(dep.to_string());
                ret.push(pkg.to_string());
            }
        }

        self.resolved.extend(new_resolved);
        ret
    }

    async fn cache_aur_pkgs_recursive2<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
        custom_pkgs: &[Targ<'_>],
        target: bool,
    ) -> Result<Vec<String>, Error> {
        if pkgs.is_empty() {
            return Ok(Vec::new());
        }

        // Compilier Bug?
        /*if log_enabled!(Debug) {
            debug!(
                "cache_aur_pkgs_recursive {:?}",
                pkgs.iter().map(|p| p.as_ref()).collect::<Vec<_>>()
            )
        }*/

        let pkgs = custom_pkgs
            .iter()
            .flat_map(|p| self.find_aur_deps_of_custom(*p))
            .chain(pkgs.iter().map(|p| p.as_ref().to_string()))
            .collect::<Vec<_>>();

        let pkgs = self.cache_aur_pkgs(&pkgs, target).await?;
        if self.flags.contains(Flags::NO_DEPS) {
            return Ok(Vec::new());
        }

        let mut new_pkgs = Vec::new();
        for pkg in pkgs {
            let check = if self.flags.contains(Flags::CHECK_DEPENDS) {
                Some(&pkg.check_depends)
            } else {
                None
            };

            let depends = pkg
                .depends
                .iter()
                .chain(&pkg.make_depends)
                .chain(check.into_iter().flatten());

            for pkg in depends {
                let dep = Depend::new(pkg.as_str());

                if (!self.flags.contains(Flags::LOCAL_REPO)
                    && self.find_repo_satisfier_silent(dep.to_string()).is_some()
                    && self.satisfied_local(&dep))
                    || self.find_repo_satisfier_silent(&pkg).is_some()
                    || self.resolved.contains(&dep.to_string())
                    || self.assume_installed(&dep)
                {
                    if log_enabled!(Debug) {
                        debug!(
                            "{} is satisfied so skipping: local={} repo={} resolved={}",
                            dep.to_string(),
                            self.flags.contains(Flags::LOCAL_REPO) && self.satisfied_local(&dep),
                            self.find_repo_satisfier_silent(&pkg).is_some(),
                            self.resolved.contains(&dep.to_string())
                        );
                    }

                    continue;
                }

                self.resolved.insert(dep.to_string());
                new_pkgs.push(pkg.clone());
            }
        }

        Ok(new_pkgs)
    }

    fn satisfied_build(&self, target: &Dep) -> bool {
        for build in &self.actions.build {
            match build {
                Base::Aur(pkgs) => {
                    if pkgs.pkgs.iter().any(|b| {
                        b.pkg
                            .satisfies_dep(target, self.flags.contains(Flags::NO_DEP_VERSION))
                    }) {
                        return true;
                    }
                }
                Base::Custom(pkgs) => {
                    if pkgs.pkgs.iter().any(|pkg| {
                        (&*pkgs.srcinfo, &pkg.pkg)
                            .satisfies_dep(target, self.flags.contains(Flags::NO_DEP_VERSION))
                    }) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn satisfied_install(&self, target: &Dep) -> bool {
        self.actions.install.iter().any(|install| {
            install
                .pkg
                .satisfies_dep(target, self.flags.contains(Flags::NO_DEP_VERSION))
        })
    }

    fn satisfied_local(&self, target: &Dep) -> bool {
        if let Ok(pkg) = self.alpm.localdb().pkg(target.name()) {
            if pkg.satisfies_dep(target, self.flags.contains(Flags::NO_DEP_VERSION)) {
                return true;
            }
        }

        if self.flags.contains(Flags::NO_DEP_VERSION) {
            let ret = self.alpm.localdb().pkgs().find_satisfier(target.name());
            ret.is_some()
        } else {
            let ret = self
                .alpm
                .localdb()
                .pkgs()
                .find_satisfier(target.to_string());
            ret.is_some()
        }
    }

    fn find_repo_satisfier<S: AsRef<str>>(&self, target: S) -> Option<alpm::Package<'a>> {
        let mut target = target.as_ref();

        if self.flags.contains(Flags::NO_DEP_VERSION) {
            target = target.split_once(is_ver_char).map_or(target, |x| x.0)
        }

        self.alpm.syncdbs().find_satisfier(target)
    }

    fn find_repo_satisfier_silent<S: AsRef<str>>(&self, target: S) -> Option<alpm::Package<'a>> {
        let cb = self.alpm.take_raw_question_cb();
        let pkg = self.find_repo_satisfier(target);
        self.alpm.set_raw_question_cb(cb);
        pkg
    }

    fn dep_is_aur_targ(&self, targs: &[&str], dep: &Dep) -> bool {
        if let Some(pkg) = self.find_satisfier_aur_cache(dep) {
            for &targ in targs {
                if pkg.satisfies_dep(
                    &Depend::new(targ),
                    self.flags.contains(Flags::NO_DEP_VERSION),
                ) {
                    return true;
                }
            }
        }

        false
    }

    fn push_aur_build(&mut self, pkgbase: &str, pkg: AurPackage) {
        debug!("pushing to build: {}", pkg.pkg.name);
        for base in &mut self.actions.build {
            if let Base::Aur(pkgs) = base {
                if pkgs.pkgs[0].pkg.package_base == pkgbase {
                    pkgs.pkgs.push(pkg);
                    return;
                }
            }
        }

        self.actions
            .build
            .push(Base::Aur(AurBase { pkgs: vec![pkg] }));
    }

    // TODO: multiple packages may have same pkgbase
    fn push_custom_build(&mut self, repo: String, base: srcinfo::Srcinfo, pkg: CustomPackage) {
        debug!("pushing to build: {}", pkg.pkg.pkgname);
        for build in &mut self.actions.build {
            if let Base::Custom(pkgs) = build {
                if pkgs.srcinfo.base.pkgbase == base.base.pkgbase {
                    pkgs.pkgs.push(pkg);
                    return;
                }
            }
        }

        self.actions.build.push(Base::Custom(CustomPackages {
            repo,
            srcinfo: Box::new(base),
            pkgs: vec![pkg],
        }));
    }

    fn calculate_make(&mut self) {
        let mut runtime = Vec::new();
        let mut run = true;
        let no_dep_ver = self.flags.contains(Flags::NO_DEP_VERSION);
        let arch = self.alpm.architectures().first();

        self.actions
            .install
            .iter()
            .filter(|p| !p.make)
            .for_each(|p| runtime.extend(p.pkg.depends().iter().map(|d| d.to_depend())));
        self.actions
            .iter_aur_pkgs()
            .filter(|p| !p.make)
            .for_each(|p| runtime.extend(p.pkg.depends.iter().map(|d| Depend::new(d.as_str()))));
        self.actions
            .iter_custom_pkgs()
            .filter(|p| !p.1.make)
            .for_each(|p| {
                runtime.extend(
                    p.1.pkg
                        .depends
                        .iter()
                        .filter(|d| {
                            d.arch.is_none()
                                || d.arch.as_deref() == self.alpm.architectures().first()
                        })
                        .flat_map(|d| &d.vec)
                        .map(|d| Depend::new(d.as_str())),
                )
            });

        runtime.sort_unstable_by_key(|a| a.to_string());
        runtime.dedup_by_key(|a| a.to_string());

        while run {
            run = false;
            for pkg in &mut self.actions.install {
                if !pkg.make {
                    continue;
                }

                let satisfied = runtime
                    .iter()
                    .any(|dep| pkg.pkg.satisfies_dep(dep, no_dep_ver));

                if satisfied {
                    pkg.make = false;
                    run = true;
                    runtime.extend(pkg.pkg.depends().iter().map(|d| d.to_depend()));
                }
            }

            for base in &mut self.actions.build {
                match base {
                    Base::Aur(base) => {
                        for pkg in &mut base.pkgs {
                            if !pkg.make {
                                continue;
                            }

                            let satisfied = runtime
                                .iter()
                                .any(|dep| pkg.pkg.satisfies_dep(dep, no_dep_ver));

                            if satisfied {
                                pkg.make = false;
                                run = true;
                                runtime.extend(
                                    pkg.pkg.depends.iter().map(|d| Depend::new(d.as_str())),
                                );
                            }
                        }
                    }
                    Base::Custom(pkgs) => {
                        for pkg in &mut pkgs.pkgs {
                            if !pkg.make {
                                continue;
                            }

                            let satisfied = runtime.iter().any(|dep| {
                                (&*pkgs.srcinfo, &pkg.pkg).satisfies_dep(dep, no_dep_ver)
                            });

                            if satisfied {
                                pkg.make = false;
                                run = true;
                                runtime.extend(
                                    pkg.pkg
                                        .depends
                                        .iter()
                                        .filter(|d| d.arch.is_none() || d.arch.as_deref() == arch)
                                        .flat_map(|d| &d.vec)
                                        .map(|d| Depend::new(d.as_str())),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn assume_installed(&self, dep: &Dep) -> bool {
        let nover = self.flags.contains(Flags::NO_DEP_VERSION);
        self.alpm
            .assume_installed()
            .iter()
            .any(|assume| satisfies_provide(dep, &assume, nover))
    }
}

fn is_ver_char(c: char) -> bool {
    matches!(c, '<' | '=' | '>')
}

fn split_pkgname(c: char) -> bool {
    matches!(c, '-' | '_' | '>')
}

fn new_want(pkg: String, dep: String) -> Want {
    Want {
        dep: (pkg != dep).then(|| dep),
        pkg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use crate::Conflict;
    use alpm::SigLevel;
    use simplelog::{ColorChoice, ConfigBuilder, LevelFilter, TermLogger, TerminalMode};

    struct TestActions {
        build: Vec<String>,
        install: Vec<String>,
        missing: Vec<Vec<String>>,
        make: usize,
        duplicates: Vec<String>,
        targets: Vec<String>,
    }

    fn _init_logger() {
        let _ = TermLogger::init(
            LevelFilter::Trace,
            ConfigBuilder::new()
                .add_filter_allow_str("aur_depends")
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Never,
        );
    }

    fn alpm() -> Alpm {
        //let handle = Alpm::new("/", "/var/lib/pacman/").unwrap();
        let mut handle = Alpm::new("/", "tests/db").unwrap();
        handle.register_syncdb("core", SigLevel::NONE).unwrap();
        handle.register_syncdb("extra", SigLevel::NONE).unwrap();
        handle.register_syncdb("community", SigLevel::NONE).unwrap();
        handle.register_syncdb("multilib", SigLevel::NONE).unwrap();
        handle
            .add_assume_installed(&Depend::new("assume-dep1"))
            .unwrap();
        handle.add_assume_installed(&Depend::new("i3-wm")).unwrap();
        handle
    }

    async fn resolve(pkgs: &[&str], flags: Flags) -> TestActions {
        //_init_logger();
        let raur = raur();
        let alpm = alpm();
        let mut cache = HashSet::new();

        let repo = vec![Repo {
            name: "my_repo".to_string(),
            pkgs: vec![srcinfo::Srcinfo::parse_file("tests/srcinfo/custom.SRCINFO").unwrap()],
        }];

        let handle = Resolver::new(&alpm, repo, &mut cache, &raur, flags)
            .aur_namespace(true)
            .provider_callback(|_, pkgs| {
                debug!("provider choice: {:?}", pkgs);
                if let Some(i) = pkgs.iter().position(|pkg| *pkg == "yay-bin") {
                    i
                } else {
                    0
                }
            });

        let actions = handle.resolve_targets(pkgs).await.unwrap();

        let mut build = actions
            .iter_aur_pkgs()
            .map(|p| p.pkg.name.clone())
            .collect::<Vec<_>>();
        build.extend(actions.iter_custom_pkgs().map(|p| p.1.pkg.pkgname.clone()));

        let mut install = actions
            .install
            .iter()
            .map(|b| b.pkg.name().to_string())
            .collect::<Vec<_>>();

        build.sort();
        install.sort();

        let make = actions.install.iter().filter(|i| i.make).count()
            + actions.iter_aur_pkgs().filter(|i| i.make).count()
            + actions.iter_custom_pkgs().filter(|i| i.1.make).count();

        let mut targets = actions
            .iter_aur_pkgs()
            .filter(|pkg| pkg.target)
            .map(|pkg| pkg.pkg.name.to_string())
            .collect::<Vec<_>>();

        targets.extend(
            actions
                .iter_custom_pkgs()
                .filter(|pkg| pkg.1.target)
                .map(|pkg| pkg.1.pkg.pkgname.to_string()),
        );

        TestActions {
            duplicates: actions.duplicate_targets(),
            install,
            build,
            missing: actions
                .missing
                .into_iter()
                .map(|m| {
                    m.stack
                        .into_iter()
                        .map(|s| s.pkg)
                        .chain(Some(m.dep))
                        .collect()
                })
                .collect(),
            make,
            targets,
        }
    }

    #[tokio::test]
    async fn test_yay() {
        let TestActions {
            install,
            build,
            make,
            ..
        } = resolve(&["yay"], Flags::new()).await;

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
        assert_eq!(make, 1);
    }

    #[tokio::test]
    async fn test_yay_needed() {
        let TestActions { install, build, .. } =
            resolve(&["yay"], Flags::new() | Flags::NEEDED).await;

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
    }

    #[tokio::test]
    async fn test_yay_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["yay"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_aur_yay_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["aur/yay"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_core_yay_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_core_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/glibc"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, vec!["glibc"]);
    }

    #[tokio::test]
    async fn test_aur_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_extra_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS).await;

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_yay_no_provides() {
        let TestActions { install, build, .. } =
            resolve(&["yay"], Flags::new() & !Flags::TARGET_PROVIDES).await;

        assert_eq!(build, vec!["yay"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
    }

    #[tokio::test]
    async fn test_make_only() {
        let TestActions { make, .. } = resolve(
            &["ros-melodic-desktop-full"],
            Flags::new() & !Flags::TARGET_PROVIDES & !Flags::MISSING_PROVIDES,
        )
        .await;
        assert_eq!(make, 40);
    }

    #[tokio::test]
    async fn test_cache_only() {
        let raur = raur();
        let alpm = alpm();
        let mut cache = HashSet::new();

        let mut handle = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::new());
        handle
            .cache_aur_pkgs_recursive(&["ros-melodic-desktop-full"], &[], true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_pacaur() {
        let TestActions { install, build, .. } = resolve(&["pacaur"], Flags::new()).await;
        assert_eq!(build, vec!["auracle-git", "pacaur"]);
        assert_eq!(
            install,
            vec![
                "git",
                "jq",
                "libnsl",
                "meson",
                "ninja",
                "oniguruma",
                "perl-error",
                "perl-mailtools",
                "perl-timedate",
                "python",
                "python-appdirs",
                "python-packaging",
                "python-pyparsing",
                "python-setuptools",
                "python-six"
            ]
        );
    }

    #[tokio::test]
    async fn test_custom() {
        let TestActions { install, build, .. } = resolve(&["custom"], Flags::new()).await;
        assert_eq!(build, vec!["c1", "c2", "c3", "custom"]);
        assert_eq!(install, vec!["libedit", "llvm-libs", "rust"]);
    }

    #[tokio::test]
    async fn test_assume() {
        let TestActions { install, build, .. } = resolve(&["assume-test"], Flags::new()).await;
        assert_eq!(build, vec!["assume-dep2", "assume-test"]);
        assert_eq!(install, vec!["libev"]);
    }

    #[tokio::test]
    async fn test_pacaur_needed() {
        let TestActions { install, build, .. } =
            resolve(&["pacaur"], Flags::new() | Flags::NEEDED).await;
        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_many() {
        let TestActions { install, build, .. } = resolve(
            &["yay", "pacaur", "pacman", "glibc", "0ad", "spotify"],
            Flags::new() | Flags::NO_DEPS,
        )
        .await;

        assert_eq!(build, vec!["pacaur", "spotify", "yay-bin"]);
        assert_eq!(install, vec!["0ad", "glibc", "pacman"]);
    }

    #[tokio::test]
    async fn test_many_needed() {
        let TestActions { install, build, .. } = resolve(
            &["yay", "pacaur", "pacman", "glibc", "0ad", "spotify"],
            Flags::new() | Flags::NO_DEPS | Flags::NEEDED,
        )
        .await;

        assert_eq!(build, vec!["spotify", "yay-bin"]);
        assert_eq!(install, vec!["0ad", "glibc"]);
    }

    #[tokio::test]
    async fn test_a() {
        let TestActions { missing, .. } = resolve(&["a"], Flags::new()).await;

        assert_eq!(missing, vec![vec!["a", "b>1"]]);
    }

    #[tokio::test]
    async fn test_a_no_ver() {
        let TestActions { build, .. } = resolve(&["a"], Flags::new() | Flags::NO_DEP_VERSION).await;

        assert_eq!(build, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_discord() {
        let TestActions {
            make,
            install,
            build,
            ..
        } = resolve(&["discord-canary"], Flags::new()).await;

        println!("{:?}", build);
        println!("{:?}", install);

        assert_eq!(build.len(), 3);
        assert_eq!(install.len(), 89 + 13);
        assert_eq!(make, 9);
    }

    #[tokio::test]
    async fn test_aur_only() {
        let TestActions { build, install, .. } =
            resolve(&["xterm", "yay"], Flags::aur_only() | Flags::NO_DEPS).await;
        assert_eq!(build, vec!["xterm", "yay-bin"]);
        assert_eq!(install, Vec::<String>::new());

        let TestActions { install, .. } =
            resolve(&["pacman"], Flags::aur_only() | Flags::NO_DEPS).await;
        assert_eq!(install, Vec::<String>::new());
        //1assert_eq!(build, vec!["pacman-git"]);
    }

    #[tokio::test]
    async fn test_repo_only() {
        let TestActions { build, install, .. } = resolve(
            &["xterm", "yay"],
            (Flags::new() | Flags::NO_DEPS) & !Flags::AUR,
        )
        .await;
        assert_eq!(install, vec!["xterm"]);
        assert_eq!(build, Vec::<String>::new());

        let TestActions { install, build, .. } =
            resolve(&["pacman"], (Flags::new() | Flags::NO_DEPS) & !Flags::AUR).await;
        assert_eq!(install, vec!["pacman"]);
        assert_eq!(build, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_dups() {
        let TestActions {
            build,
            install,
            duplicates,
            ..
        } = resolve(&["extra/xterm", "aur/xterm"], Flags::new()).await;

        println!("{:#?}", install);
        println!("{:#?}", build);
        assert_eq!(duplicates.len(), 1);
    }

    #[tokio::test]
    async fn test_inner_conflicts() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let handle = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::new());
        let actions = handle
            .resolve_targets(&["yay", "yay-git", "yay-bin"])
            .await
            .unwrap();

        let mut conflict1 = Conflict::new("yay".into());
        conflict1.push("yay-git".into(), &Depend::new("yay"));
        conflict1.push("yay-bin".into(), &Depend::new("yay"));
        let mut conflict2 = Conflict::new("yay-bin".into());
        conflict2.push("yay-git".into(), &Depend::new("yay"));
        let mut conflict3 = Conflict::new("yay-git".into());
        conflict3.push("yay-bin".into(), &Depend::new("yay"));

        assert_eq!(
            actions.calculate_inner_conflicts(true),
            vec![conflict1, conflict2, conflict3]
        );
    }

    #[tokio::test]
    async fn test_conflicts() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let handle = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::new());
        let actions = handle.resolve_targets(&["pacman-git"]).await.unwrap();

        let mut conflict = Conflict::new("pacman-git".into());
        conflict.push("pacman".into(), &Depend::new("pacman"));

        assert_eq!(actions.calculate_conflicts(true), vec![conflict]);
    }

    #[tokio::test]
    async fn test_aur_updates() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let mut handle = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, Flags::new());
        let pkgs = handle.aur_updates().await.unwrap().updates;
        let pkgs = pkgs
            .iter()
            .map(|p| p.remote.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(pkgs, vec!["version_newer"]);
    }

    #[tokio::test]
    async fn test_aur_updates_enable_downgrade() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let mut handle = Resolver::new(
            &alpm,
            Vec::new(),
            &mut cache,
            &raur,
            Flags::new() | Flags::ENABLE_DOWNGRADE,
        );
        let pkgs = handle.aur_updates().await.unwrap().updates;
        let pkgs = pkgs
            .iter()
            .map(|p| p.remote.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(pkgs, vec!["pacaur", "version_newer", "version_older"]);
    }

    #[tokio::test]
    async fn test_repo_nover() {
        let TestActions { install, .. } = resolve(&["repo_version_test"], Flags::new()).await;
        assert_eq!(install, Vec::<String>::new());

        let TestActions { install, .. } =
            resolve(&["repo_version_test"], Flags::new() | Flags::NO_DEP_VERSION).await;
        assert_eq!(install, vec!["pacman-contrib"]);
    }

    #[tokio::test]
    async fn test_satisfied_versioned_repo_dep() {
        let TestActions { missing, .. } =
            resolve(&["satisfied_versioned_repo_dep"], Flags::new()).await;
        assert_eq!(
            missing,
            vec![vec!["satisfied_versioned_repo_dep", "pacman>100"]]
        );

        let TestActions { missing, .. } = resolve(
            &["satisfied_versioned_repo_dep"],
            Flags::new() | Flags::NO_DEP_VERSION,
        )
        .await;
        assert_eq!(missing, Vec::<Vec<String>>::new());
    }

    #[tokio::test]
    async fn test_satisfied_versioned_repo_dep_nover() {
        let TestActions { build, install, .. } = resolve(
            &["satisfied_versioned_repo_dep"],
            Flags::new() | Flags::NO_DEP_VERSION,
        )
        .await;
        assert_eq!(build, vec!["satisfied_versioned_repo_dep"]);
        assert!(install.is_empty());

        let TestActions { missing, .. } = resolve(
            &["satisfied_versioned_repo_dep"],
            Flags::new() | Flags::NO_DEP_VERSION,
        )
        .await;
        assert_eq!(missing, Vec::<Vec<String>>::new());
    }

    #[tokio::test]
    async fn test_cyclic() {
        let TestActions { .. } = resolve(&["cyclic"], Flags::new()).await;
    }

    #[tokio::test]
    async fn test_cyclic2() {
        let TestActions { .. } = resolve(&["systemd-git"], Flags::new()).await;
    }

    #[tokio::test]
    async fn test_resolve_targets() {
        let raur = raur();
        //let raur = raur::Handle::default();
        let alpm = alpm();
        let mut cache = HashSet::new();
        let flags = Flags::new() & !Flags::TARGET_PROVIDES & !Flags::MISSING_PROVIDES;

        let handle = Resolver::new(&alpm, Vec::new(), &mut cache, &raur, flags).provider_callback(
            |_, pkgs| {
                println!("provider choice: {:?}", pkgs);
                0
            },
        );

        let actions = handle
            .resolve_targets(&["ros-melodic-desktop-full"])
            .await
            //.resolve_targets(&["yay", "yay-bin", "yay-git"])
            //.resolve_targets(&["yay", "pikaur", "pacman", "glibc", "0ad", "spotify"])
            //.resolve_targets(&["0ad"])
            //.resolve_targets(&["linux-pf"])
            //.resolve_targets(&["ros-melodic-desktop-full", "yay"])
            //.resolve_targets(&["ignition-common"])
            .unwrap();

        actions
            .iter_aur_pkgs()
            .for_each(|p| println!("b {}", p.pkg.name));
        actions
            .iter_custom_pkgs()
            .for_each(|p| println!("c {}", p.1.pkg.pkgname));

        actions
            .install
            .iter()
            .for_each(|p| println!("i {}", p.pkg.name()));

        actions
            .missing
            .iter()
            .for_each(|m| println!("missing {:?}", m));

        actions.calculate_conflicts(true).iter().for_each(|c| {
            println!("c {}: ", c.pkg);
            c.conflicting
                .iter()
                .for_each(|c| println!("    {} ({:?})", c.pkg, c.conflict))
        });

        actions
            .calculate_inner_conflicts(true)
            .iter()
            .for_each(|c| {
                println!("c {}: ", c.pkg);
                c.conflicting
                    .iter()
                    .for_each(|c| println!("    {} ({:?})", c.pkg, c.conflict))
            });

        actions
            .unneeded
            .iter()
            .for_each(|p| println!("u {}", p.name));

        actions
            .duplicate_targets()
            .iter()
            .for_each(|p| println!("d {}", p));

        println!(
            "build: {}",
            actions.iter_aur_pkgs().count() + actions.iter_custom_pkgs().count()
        );

        println!("install: {}", actions.install.len());
    }

    #[tokio::test]
    async fn test_target_flags() {
        let TestActions { targets, .. } = resolve(&["discord-canary"], Flags::new()).await;
        assert_eq!(targets, vec!["discord-canary"]);
    }
}
