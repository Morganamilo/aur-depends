use crate::actions::{
    Actions, AurPackage, AurUpdate, AurUpdates, Base, Conflict, Missing, RepoPackage,
};
use crate::satisfies::{satisfies_aur_pkg, satisfies_repo_pkg};
use crate::Error;
use bitflags::bitflags;

use std::collections::{HashMap, HashSet};
use std::fmt;

use alpm::{Alpm, Depend, Version};
use alpm_utils::{DbListExt, Target};
use log::Level::Trace;
use log::{debug, error, log_enabled, trace};
use raur::{Raur, SearchBy};
use raur_ext::{Cache, RaurExt};

type ConflictMap = HashMap<String, Conflict>;

bitflags! {
    /// Config options for Handle.
    pub struct Flags: u16 {
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
        /// Calculate conflicts
        const CALCULATE_CONFLICTS = 1 << 8;
        /// Calculate conflicts.
        const CHECK_DEPENDS = 1 << 9;
        /// Ignore targets that are up to date.
        const NEEDED = 1 << 10;
        /// Only search the AUR for targets.
        const AUR_ONLY = 1 << 11;
        /// Only search the repos for targets.
        const REPO_ONLY = 1 << 12;
        /// Allow the use of `aur/foo` as meaning from the AUR, instead of a repo named `aur`.
        const AUR_NAMESPACE = 1 << 13;
        /// when fetching updates, also include packages that are older than locally installed.
        const ENABLE_DOWNGRADE = 1 << 14;
    }
}

impl Flags {
    /// Create a new Flags with the default configuration
    pub fn new() -> Self {
        Flags::CALCULATE_MAKE
            | Flags::TARGET_PROVIDES
            | Flags::MISSING_PROVIDES
            | Flags::AUR_NAMESPACE
            | Flags::CALCULATE_CONFLICTS
            | Flags::CHECK_DEPENDS
    }
}

impl Default for Flags {
    fn default() -> Self {
        Self::new()
    }
}

struct ProviderCallback(Box<dyn Fn(&[&str]) -> usize>);

impl fmt::Debug for ProviderCallback {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("ProviderCallback")
    }
}

impl ProviderCallback {
    fn new<F: Fn(&[&str]) -> usize + 'static>(f: F) -> Self {
        ProviderCallback(Box::new(f))
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
/// # fn run() -> Result<(), Error> {
/// use std::collections::HashSet;
/// use alpm::Alpm;
/// use raur::Handle;
///
/// use aur_depends::{Flags, Resolver};
///
/// let alpm = Alpm::new("/", "/var/lib/pacman")?;
/// let raur = Handle::default();
/// let mut cache = HashSet::new();
/// let resolver = Resolver::new(&alpm, &mut cache, &raur, Flags::new() | Flags::AUR_ONLY);
/// let actions = resolver.resolve_targets(&["discord-canary", "spotify"])?;
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
pub struct Resolver<'a, H: Raur = raur::Handle> {
    alpm: &'a Alpm,
    resolved: HashSet<String>,
    cache: &'a mut Cache,
    stack: Vec<String>,
    raur: &'a H,
    actions: Actions<'a>,
    seen: HashSet<String>,
    flags: Flags,
    provider_callback: Option<ProviderCallback>,
}

impl<'a, H> Resolver<'a, H>
where
    H: Raur<Err = raur::Error>,
{
    /// Create a new Resolver
    pub fn new(alpm: &'a Alpm, cache: &'a mut Cache, raur: &'a H, flags: Flags) -> Self {
        Resolver {
            alpm,
            resolved: HashSet::new(),
            cache,
            stack: Vec::new(),
            actions: Actions::default(),
            raur,
            flags,
            seen: HashSet::new(),
            provider_callback: None,
        }
    }

    /// Set the provider callback
    ///
    /// The provider callback will be called any time there is a choice of multiple AUR packages
    /// that can satisfy a dependency. This callback receives a slice of package names then, returns
    /// the index of which package to pick.
    ///
    /// Retuning an invalid index will cause a panic.
    pub fn provider_callback<F: Fn(&[&str]) -> usize + 'static>(mut self, f: F) -> Self {
        self.provider_callback = Some(ProviderCallback::new(f));
        self
    }

    /// Get which aur packages need to be updated.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use aur_depends::{Error, AurUpdates};
    /// # fn run() -> Result<(), Error> {
    /// use std::collections::HashSet;
    /// use alpm::Alpm;
    /// use raur::Handle;
    ///
    /// use aur_depends::{Flags, Resolver};
    ///
    /// let alpm = Alpm::new("/", "/var/lib/pacman")?;
    /// let raur = Handle::default();
    /// let mut cache = HashSet::new();
    /// let mut resolver = Resolver::new(&alpm, &mut cache, &raur, Flags::new() | Flags::AUR_ONLY);
    ///
    /// let updates = resolver.aur_updates()?;
    ///
    /// for update in updates.updates {
    ///     println!("update: {}: {} -> {}", update.local.name(), update.local.version(),
    ///     update.remote.version);
    /// }
    /// # Ok (())
    /// # }
    /// ```
    pub fn aur_updates(&mut self) -> Result<AurUpdates<'a>, Error> {
        let local_pkgs = self
            .alpm
            .localdb()
            .pkgs()?
            .filter(|p| self.alpm.syncdbs().find_satisfier(p.name()).is_none())
            .filter(|pkg| !pkg.should_ignore())
            .collect::<Vec<_>>();

        let local_pkg_names = local_pkgs.iter().map(|pkg| pkg.name()).collect::<Vec<_>>();
        self.raur.cache_info(self.cache, &local_pkg_names)?;
        let mut missing = Vec::new();

        let to_upgrade = local_pkgs
            .into_iter()
            .filter_map(|local_pkg| {
                if let Some(pkg) = self.cache.get(local_pkg.name()) {
                    let should_upgrade = if self.flags.contains(Flags::ENABLE_DOWNGRADE) {
                        Version::new(&pkg.version) != local_pkg.version()
                    } else {
                        Version::new(&pkg.version) > local_pkg.version()
                    };

                    if should_upgrade {
                        let up = AurUpdate {
                            local: local_pkg,
                            remote: pkg.clone(),
                        };
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
        };
        Ok(updates)
    }

    /// Resolve a list of targets.
    pub fn resolve_targets(mut self, pkgs: &[&str]) -> Result<Actions<'a>, Error> {
        let mut aur_targets = Vec::new();
        let mut repo_targets = Vec::new();
        let localdb = self.alpm.localdb();

        for pkg in pkgs {
            let pkg = Target::from(pkg);
            if pkg.repo == Some("aur") && self.flags.contains(Flags::AUR_NAMESPACE) {
                aur_targets.push(pkg.pkg);
                continue;
            }

            if !self.flags.contains(Flags::AUR_ONLY) {
                if let Some(alpm_pkg) = self.alpm.syncdbs().find_target_satisfier(pkg)? {
                    repo_targets.push((pkg, alpm_pkg));
                    continue;
                }
            }

            if pkg.repo.is_none() && !self.flags.contains(Flags::REPO_ONLY) {
                aur_targets.push(pkg.pkg);
                continue;
            }

            self.actions.missing.push(Missing {
                dep: pkg.to_string(),
                stack: Vec::new(),
            });
        }

        self.cache_aur_pkgs_recursive(&aur_targets, true)?;

        for (pkg, alpm_pkg) in repo_targets {
            let mut up_to_date = false;

            if self.flags.contains(Flags::NEEDED) {
                if let Ok(local) = localdb.pkg(alpm_pkg.name()) {
                    if local.version() >= alpm_pkg.version() {
                        up_to_date = true
                    }
                }
            }

            if up_to_date {
                self.actions.unneeded.push(pkg.to_string());
            } else {
                self.resolve_repo_pkg(alpm_pkg, true)?;
            }
        }

        for aur_pkg in aur_targets {
            let dep = Depend::new(aur_pkg);
            let pkg = if let Some(pkg) = self.select_satisfier_aur_cache(&dep, true)? {
                pkg.clone()
            } else {
                self.actions.missing.push(Missing {
                    dep: dep.to_string(),
                    stack: self.stack.clone(),
                });
                continue;
            };

            let mut up_to_date = false;

            if self.flags.contains(Flags::NEEDED) {
                if let Ok(local) = localdb.pkg(&pkg.name) {
                    if local.version() >= Version::new(&pkg.version) {
                        up_to_date = true
                    }
                }
            }

            if up_to_date {
                self.actions.unneeded.push(aur_pkg.to_string());
                continue;
            }

            self.stack.push(aur_pkg.to_string());
            self.resolve_aur_pkg(&pkg)?;
            self.stack.pop().unwrap();

            if self
                .actions
                .iter_build_pkgs()
                .any(|p| p.pkg.name == pkg.name)
            {
                continue;
            }

            let p = AurPackage {
                pkg: pkg.clone(),
                make: false,
                target: true,
            };

            self.push_build(&pkg.package_base, p);
        }

        if self.flags.contains(Flags::CALCULATE_MAKE) {
            self.calculate_make();
        }

        if self.flags.contains(Flags::CALCULATE_CONFLICTS) {
            self.calculate_conflicts()?;
        }

        self.calculate_dups();

        Ok(self.actions)
    }

    fn find_satisfier_aur_cache(&self, dep: &Depend) -> Option<&raur_ext::Package> {
        if let Some(pkg) = self.cache.get(dep.name()) {
            if satisfies_aur_pkg(dep, pkg, self.flags.contains(Flags::NO_DEP_VERSION)) {
                return Some(pkg);
            }
        }

        self.cache
            .iter()
            .find(|pkg| satisfies_aur_pkg(dep, pkg, self.flags.contains(Flags::NO_DEP_VERSION)))
    }

    /// Expected behaviour
    /// pull in a list of all matches, if one is installed, default to it.
    /// unless we are looking for a target, then always show all options.
    fn select_satisfier_aur_cache(
        &self,
        dep: &Depend,
        target: bool,
    ) -> Result<Option<&raur_ext::Package>, Error> {
        if let Some(f) = &self.provider_callback {
            let mut pkgs = self
                .cache
                .iter()
                .filter(|pkg| {
                    satisfies_aur_pkg(dep, pkg, self.flags.contains(Flags::NO_DEP_VERSION))
                })
                .map(|pkg| pkg.name.as_str())
                .collect::<Vec<_>>();

            debug!("satisfiers for '{:?}': {:?})", dep.to_string(), pkgs);

            if pkgs.len() == 1 {
                return Ok(self.cache.get(pkgs[0]));
            } else if pkgs.is_empty() {
                return Ok(None);
            }

            if !target {
                for &pkg in &pkgs {
                    if self.alpm.localdb().pkg(pkg).is_ok() {
                        return Ok(self.cache.get(pkg));
                    }
                }
            }

            if let Some(true_pkg) = pkgs.iter().position(|pkg| *pkg == dep.name()) {
                pkgs.swap(true_pkg, 0);
            }

            pkgs[1..].sort();

            let choice = f.0(&pkgs);
            debug!("choice was: {}={}", choice, pkgs[choice]);
            Ok(self.cache.get(pkgs[choice]))
        } else {
            Ok(self.find_satisfier_aur_cache(dep))
        }
    }

    fn resolve_aur_pkg(&mut self, pkg: &raur_ext::Package) -> Result<(), Error> {
        if !self.flags.contains(Flags::NO_DEPS) {
            let check = if self.flags.contains(Flags::CHECK_DEPENDS) {
                Some(&pkg.check_depends)
            } else {
                None
            };

            let depends = pkg
                .make_depends
                .iter()
                .chain(check.into_iter().flatten())
                .chain(&pkg.depends);

            for dep_str in depends {
                let dep = Depend::new(dep_str);

                if self.satisfied_build(&dep)
                    || self.satisfied_local(&dep)?
                    || self.satisfied_install(&dep)
                {
                    continue;
                }

                if let Some(pkg) = self.find_repo_satisfier(dep.to_string()) {
                    self.stack.push(pkg.name().to_string());
                    self.resolve_repo_pkg(pkg, false)?;
                    self.stack.pop().unwrap();
                    continue;
                }

                let pkg = if let Some(pkg) = self.select_satisfier_aur_cache(&dep, false)? {
                    pkg.clone()
                } else {
                    debug!("failed to find '{}' in aur cache", dep.to_string(),);
                    if log_enabled!(Trace) {
                        trace!(
                            "at time of failure plgcache is: {:?}\n",
                            self.cache.iter().map(|p| &p.name).collect::<Vec<_>>()
                        );
                    }
                    self.actions.missing.push(Missing {
                        dep: dep.to_string(),
                        stack: self.stack.clone(),
                    });
                    continue;
                };

                self.stack.push(dep_str.to_string());
                self.resolve_aur_pkg(&pkg)?;
                self.stack.pop();

                let p = AurPackage {
                    pkg: pkg.clone(),
                    make: true,
                    target: false,
                };

                self.push_build(&pkg.package_base, p);
            }
        }

        Ok(())
    }

    fn resolve_repo_pkg(&mut self, pkg: alpm::Package<'a>, target: bool) -> Result<(), Error> {
        if !self.seen.insert(pkg.name().to_string()) {
            return Ok(());
        }

        if !self.flags.contains(Flags::NO_DEPS) {
            for dep in pkg.depends() {
                if self.satisfied_install(&dep) || self.satisfied_local(&dep)? {
                    continue;
                }

                if let Some(pkg) = self.find_repo_satisfier(dep.to_string()) {
                    self.resolve_repo_pkg(pkg, false)?;
                } else {
                    self.actions.missing.push(Missing {
                        dep: dep.to_string(),
                        stack: Vec::new(),
                    });
                }
            }
        }

        self.actions.install.push(RepoPackage {
            pkg,
            make: !target,
            target,
        });

        Ok(())
    }

    fn cache_aur_pkgs<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
        target: bool,
    ) -> Result<Vec<raur_ext::Package>, Error> {
        let mut pkgs_nover = pkgs
            .iter()
            .map(|p| p.as_ref().split(is_ver_char).next().unwrap())
            .collect::<Vec<_>>();
        pkgs_nover.sort();
        pkgs_nover.dedup();

        if (!target && self.flags.contains(Flags::PROVIDES))
            || (target && self.flags.contains(Flags::TARGET_PROVIDES))
        {
            self.cache_provides(&pkgs_nover)
        } else {
            let mut info = self.raur.cache_info(self.cache, &pkgs_nover)?;

            if self.flags.contains(Flags::MISSING_PROVIDES) {
                let missing = pkgs
                    .iter()
                    .map(|pkg| Depend::new(pkg.as_ref()))
                    .filter(|dep| {
                        !info.iter().any(|info| {
                            satisfies_aur_pkg(dep, info, self.flags.contains(Flags::NO_DEP_VERSION))
                        })
                    })
                    .map(|dep| dep.name().to_string())
                    .collect::<Vec<_>>();

                if !missing.is_empty() {
                    debug!("attempting to find provides for missing: {:?}", missing);
                    info.extend(self.cache_provides(&missing)?);
                }
            }

            if log_enabled!(Trace) {
                debug!(
                    "provides resolved {:?} found {:?}\n",
                    pkgs.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
                    info.iter().map(|p| p.name.clone()).collect::<Vec<_>>()
                );
            }
            Ok(info)
        }
    }

    fn cache_provides<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
    ) -> Result<Vec<raur_ext::Package>, Error> {
        let mut to_info = pkgs
            .iter()
            .map(|s| s.as_ref().to_string())
            .collect::<Vec<_>>();
        for pkg in pkgs {
            let pkg = pkg.as_ref();

            // Optimization, may break when using AUR_ONLY
            // for example, trying to resolve "pacman" with AUR_ONLY should pull in
            // "pacman-git". But because pacman is installed locally, this optimization
            // causes us to not cache "pacman-git" and end up with missing.
            if self.alpm.localdb().pkg(pkg).is_ok() {
                continue;
            }

            for word in pkg.rsplitn(2, split_pkgname).last() {
                debug!("provide search: {} {}", pkg, word);
                to_info.extend(
                    //TODO: async?
                    self.raur
                        .search_by(word, SearchBy::NameDesc)
                        .unwrap_or_else(|e| {
                            error!("provide search '{}' failed: {}", word, e);
                            Vec::new()
                        })
                        .into_iter()
                        .map(|p| p.name),
                );
            }
        }

        debug!("trying to cache {:?}\n", to_info);

        let mut ret = self.raur.cache_info(self.cache, &to_info)?;

        ret.retain(|pkg| {
            pkgs.iter().any(|dep| {
                satisfies_aur_pkg(
                    &Depend::new(dep.as_ref()),
                    pkg,
                    self.flags.contains(Flags::NO_DEP_VERSION),
                )
            })
        });

        Ok(ret)
    }

    fn cache_aur_pkgs_recursive<S: AsRef<str>>(
        &mut self,
        pkgs: &[S],
        target: bool,
    ) -> Result<(), Error> {
        if pkgs.is_empty() {
            return Ok(());
        }

        let pkgs = self.cache_aur_pkgs(&pkgs, target)?;
        if self.flags.contains(Flags::NO_DEPS) {
            return Ok(());
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
                let dep = Depend::new(pkg);

                if self.satisfied_local(&dep).unwrap()
                    || self.find_repo_satisfier(&pkg).is_some()
                    || (self.find_satisfier_aur_cache(&dep).is_some()
                        || self.resolved.contains(dep.name()))
                {
                    continue;
                }

                self.resolved.insert(dep.name().to_string());
                new_pkgs.push(pkg.clone());
            }
        }

        self.cache_aur_pkgs_recursive(&new_pkgs, false)
    }

    fn satisfied_build(&self, target: &Depend) -> bool {
        self.actions
            .build
            .iter()
            .flat_map(|base| &base.pkgs)
            .any(|build| {
                satisfies_aur_pkg(
                    target,
                    &build.pkg,
                    self.flags.contains(Flags::NO_DEP_VERSION),
                )
            })
    }

    fn satisfied_install(&self, target: &Depend) -> bool {
        self.actions.install.iter().any(|install| {
            satisfies_repo_pkg(
                target,
                &install.pkg,
                self.flags.contains(Flags::NO_DEP_VERSION),
            )
        })
    }

    fn satisfied_local(&self, target: &Depend) -> Result<bool, Error> {
        if let Ok(pkg) = self.alpm.localdb().pkg(target.name()) {
            if satisfies_repo_pkg(target, &pkg, self.flags.contains(Flags::NO_DEP_VERSION)) {
                return Ok(true);
            }
        }

        if self.flags.contains(Flags::NO_DEP_VERSION) {
            let ret = self.alpm.localdb().pkgs()?.find_satisfier(target.name());
            Ok(ret.is_some())
        } else {
            let ret = self
                .alpm
                .localdb()
                .pkgs()?
                .find_satisfier(target.to_string());
            Ok(ret.is_some())
        }
    }

    fn find_repo_satisfier<S: AsRef<str>>(&self, target: S) -> Option<alpm::Package<'a>> {
        let mut target = target.as_ref();

        if self.flags.contains(Flags::NO_DEP_VERSION) {
            target = target.splitn(2, is_ver_char).next().unwrap();
        }

        self.alpm.syncdbs().find_satisfier(target)
    }

    fn push_build(&mut self, pkgbase: &str, pkg: AurPackage) {
        for base in &mut self.actions.build {
            if base.package_base() == pkgbase {
                base.pkgs.push(pkg);
                return;
            }
        }

        self.actions.build.push(Base { pkgs: vec![pkg] });
    }

    fn calculate_dups(&mut self) {
        let mut names = HashSet::new();

        let build = self
            .actions
            .build
            .iter()
            .flat_map(|b| &b.pkgs)
            .map(|pkg| pkg.pkg.name.as_str());

        self.actions.duplicates = self
            .actions
            .install
            .iter()
            .map(|pkg| pkg.pkg.name())
            .chain(build)
            .filter(|&name| !names.insert(name))
            .map(Into::into)
            .collect::<Vec<_>>();
    }

    fn calculate_make(&mut self) {
        let mut runtime = HashSet::new();
        let mut run = true;
        let no_dep_ver = self.flags.contains(Flags::NO_DEP_VERSION);

        self.actions
            .install
            .iter()
            .filter(|p| !p.make)
            .for_each(|p| runtime.extend(p.pkg.depends()));
        self.actions
            .build
            .iter()
            .flat_map(|b| &b.pkgs)
            .filter(|p| !p.make)
            .for_each(|p| runtime.extend(p.pkg.depends.iter().map(|d| Depend::new(d))));

        while run {
            run = false;
            for pkg in &mut self.actions.install {
                if !pkg.make {
                    continue;
                }

                let satisfied = runtime
                    .iter()
                    .any(|dep| satisfies_repo_pkg(dep, &pkg.pkg, no_dep_ver));

                if satisfied {
                    pkg.make = false;
                    run = true;
                    runtime.extend(pkg.pkg.depends());
                }
            }

            for base in &mut self.actions.build {
                for pkg in &mut base.pkgs {
                    if !pkg.make {
                        continue;
                    }

                    let satisfied = runtime
                        .iter()
                        .any(|dep| satisfies_aur_pkg(dep, &pkg.pkg, no_dep_ver));

                    if satisfied {
                        pkg.make = false;
                        run = true;
                        runtime.extend(pkg.pkg.depends.iter().map(|d| Depend::new(d)));
                    }
                }
            }
        }
    }
}

impl<'a, H> Resolver<'a, H>
where
    H: Raur<Err = raur::Error>,
{
    fn has_pkg<S: AsRef<str>>(&self, name: S) -> bool {
        let name = name.as_ref();
        let install = &self.actions.install;
        self.actions
            .iter_build_pkgs()
            .any(|pkg| pkg.pkg.name == name)
            || install.iter().any(|pkg| pkg.pkg.name() == name)
    }

    // check a conflict from locally installed pkgs, against install+build
    fn check_reverse_conflict<S: AsRef<str>>(
        &self,
        name: S,
        conflict: &Depend,
        conflicts: &mut ConflictMap,
    ) {
        let name = name.as_ref();

        self.actions
            .install
            .iter()
            .map(|pkg| &pkg.pkg)
            .filter(|pkg| pkg.name() != name)
            .filter(|pkg| satisfies_repo_pkg(conflict, pkg, false))
            .for_each(|pkg| {
                conflicts
                    .entry(pkg.name().to_string())
                    .or_insert_with(|| Conflict::new(pkg.name().to_string()))
                    .push(name.to_string(), conflict);
            });

        self.actions
            .iter_build_pkgs()
            .map(|pkg| &pkg.pkg)
            .filter(|pkg| pkg.name != name)
            .filter(|pkg| satisfies_aur_pkg(conflict, pkg, false))
            .for_each(|pkg| {
                conflicts
                    .entry(pkg.name.to_string())
                    .or_insert_with(|| Conflict::new(pkg.name.to_string()))
                    .push(name.to_string(), conflict);
            });
    }

    // check a conflict from install+build against all locally installed pkgs
    fn check_forward_conflict<S: AsRef<str>>(
        &self,
        name: S,
        conflict: &Depend,
        conflicts: &mut ConflictMap,
    ) -> Result<(), Error> {
        let name = name.as_ref();
        self.alpm
            .localdb()
            .pkgs()?
            .filter(|pkg| !self.has_pkg(pkg.name()))
            .filter(|pkg| pkg.name() != name)
            .filter(|pkg| satisfies_repo_pkg(conflict, pkg, false))
            .for_each(|pkg| {
                conflicts
                    .entry(name.to_string())
                    .or_insert_with(|| Conflict::new(name.to_string()))
                    .push(pkg.name().to_string(), conflict);
            });

        Ok(())
    }

    fn check_forward_conflicts(&self, conflicts: &mut ConflictMap) -> Result<(), Error> {
        for pkg in self.actions.install.iter() {
            for conflict in pkg.pkg.conflicts() {
                self.check_forward_conflict(pkg.pkg.name(), &conflict, conflicts)?;
            }
        }

        for pkg in self.actions.iter_build_pkgs() {
            for conflict in &pkg.pkg.conflicts {
                self.check_forward_conflict(&pkg.pkg.name, &Depend::new(conflict), conflicts)?;
            }
        }

        Ok(())
    }

    fn check_inner_conflicts(&self, conflicts: &mut ConflictMap) {
        for pkg in self.actions.install.iter() {
            for conflict in pkg.pkg.conflicts() {
                self.check_reverse_conflict(pkg.pkg.name(), &conflict, conflicts)
            }
        }

        for pkg in self.actions.iter_build_pkgs() {
            for conflict in pkg.pkg.conflicts.iter() {
                self.check_reverse_conflict(&pkg.pkg.name, &Depend::new(conflict), conflicts)
            }
        }
    }

    fn check_reverse_conflicts(&self, conflicts: &mut ConflictMap) -> Result<(), Error> {
        self.alpm
            .localdb()
            .pkgs()?
            .filter(|pkg| !self.has_pkg(pkg.name()))
            .for_each(|pkg| {
                pkg.conflicts().for_each(|conflict| {
                    self.check_reverse_conflict(pkg.name(), &conflict, conflicts)
                })
            });

        Ok(())
    }

    fn calculate_conflicts(&mut self) -> Result<(), Error> {
        let mut conflicts = ConflictMap::new();
        let mut inner_conflicts = ConflictMap::new();

        self.check_inner_conflicts(&mut inner_conflicts);
        self.check_reverse_conflicts(&mut conflicts)?;
        self.check_forward_conflicts(&mut conflicts)?;

        self.actions.conflicts = conflicts.into_iter().map(|(_, v)| v).collect();
        self.actions.inner_conflicts = inner_conflicts.into_iter().map(|(_, v)| v).collect();

        self.actions.conflicts.sort();
        self.actions.inner_conflicts.sort();

        Ok(())
    }
}

fn is_ver_char(c: char) -> bool {
    match c {
        '<' | '=' | '>' => true,
        _ => false,
    }
}

fn split_pkgname(c: char) -> bool {
    match c {
        '-' | '_' | '>' => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use alpm::SigLevel;
    use simplelog::{ConfigBuilder, LevelFilter, TermLogger, TerminalMode};

    struct TestActions {
        build: Vec<String>,
        install: Vec<String>,
        missing: Vec<Vec<String>>,
        make: usize,
        duplicates: Vec<String>,
    }

    fn raur() -> impl Raur<Err = raur::Error> {
        let mut raur = MockRaur::new();
        raur.pkg("a").depend("b>1");
        raur.pkg("b").version("1");

        raur.pkg("flann")
            .depend("lz4")
            .depend("hdf5")
            .make_depend("cmake")
            .make_depend("python2")
            .make_depend("texlive-core");
        raur.pkg("gazebo")
            .depend("boost>=1.40.0")
            .depend("curl>=4.0")
            .depend("freeglut")
            .depend("freeimage>=3.0")
            .depend("intel-tbb>=3.0")
            .depend("libccd>=1.4")
            .depend("libltdl>=2.4.2")
            .depend("libtar>=1.2")
            .depend("libxml2>=2.7.7")
            .depend("ogre-1.9")
            .depend("protobuf>=2.3.0")
            .depend("sdformat=6")
            .depend("ignition-math=4")
            .depend("ignition-transport=4")
            .depend("ignition-common")
            .depend("ignition-fuel_tools")
            .depend("ignition-msgs")
            .depend("tinyxml2")
            .depend("qwt")
            .make_depend("cmake")
            .make_depend("doxygen")
            .make_depend("ignition-cmake");
        raur.pkg("ignition-common")
            .depend("ignition-math>=6")
            .depend("tinyxml2")
            .depend("freeimage")
            .depend("libutil-linux")
            .depend("gts")
            .depend("ffmpeg")
            .make_depend("ignition-cmake>=2")
            .make_depend("util-linux");
        raur.pkg("ignition-cmake")
            .depend("cmake")
            .depend("pkgconfig")
            .depend("ruby-ronn")
            .depend("doxygen");
        raur.pkg("ignition-fuel_tools")
            .depend("curl")
            .depend("jsoncpp")
            .depend("libyaml")
            .depend("libzip")
            .depend("ignition-common>=3")
            .make_depend("ignition-cmake>=2");
        raur.pkg("libccd").provide("libccd").make_depend("cmake");
        raur.pkg("opencv3-opt")
            .provide("opencv3")
            .depend("intel-tbb")
            .depend("openexr")
            .depend("gst-plugins-base")
            .depend("libdc1394")
            .depend("cblas")
            .depend("lapack")
            .depend("libgphoto2")
            .depend("jasper")
            .depend("ffmpeg")
            .make_depend("cmake")
            .make_depend("python-numpy")
            .make_depend("python-setuptools")
            .make_depend("mesa")
            .make_depend("eigen")
            .make_depend("hdf5")
            .make_depend("lapacke")
            .make_depend("gtk3")
            .make_depend("vtk")
            .make_depend("glew")
            .make_depend("double-conversion");
        raur.pkg("pcl")
            .depend("boost")
            .depend("eigen")
            .depend("flann")
            .depend("vtk")
            .depend("qhull")
            .depend("qt5-base")
            .depend("glu")
            .depend("qt5-webkit")
            .depend("openmpi")
            .depend("python2")
            .depend("libxt")
            .depend("libharu")
            .depend("proj")
            .depend("glew")
            .depend("netcdf")
            .depend("libusb")
            .make_depend("cmake")
            .make_depend("gl2ps")
            .make_depend("python");
        raur.pkg("python-catkin_pkg")
            .provide("python-catkin-pkg")
            .depend("python")
            .depend("python-argparse")
            .depend("python-dateutil")
            .depend("python-docutils")
            .make_depend("python-setuptools");
        raur.pkg("python-empy").depend("python");
        raur.pkg("python-rosdep")
            .depend("python")
            .depend("python-catkin_pkg")
            .depend("python-rosdistro")
            .depend("python-rospkg")
            .depend("python-yaml")
            .make_depend("python-setuptools");
        raur.pkg("python-rosdistro")
            .depend("python")
            .depend("python-catkin_pkg")
            .depend("python-rospkg")
            .depend("python-yaml")
            .make_depend("python-setuptools");
        raur.pkg("python-rospkg")
            .depend("python")
            .depend("python-yaml")
            .make_depend("python-setuptools");
        raur.pkg("ros-build-tools");
        raur.pkg("ros-build-tools-py3")
            .provide("ros-build-tools")
            .depend("bash");
        raur.pkg("ros-melodic-camera-calibration")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-image-geometry")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-camera-calibration-parsers")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-roscpp-serialization")
            .depend("boost")
            .depend("yaml-cpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("yaml-cpp")
            .make_depend("boost")
            .make_depend("pkg-config");
        raur.pkg("ros-melodic-camera-info-manager")
            .depend("ros-melodic-camera-calibration-parsers")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-image-transport")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-roslib")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-camera-calibration-parsers")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-image-transport")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-roslib")
            .make_depend("boost");
        raur.pkg("ros-melodic-catkin")
            .depend("python-nose")
            .depend("gtest")
            .depend("python-catkin_pkg")
            .depend("python-empy")
            .depend("gmock")
            .depend("python")
            .depend("ros-build-tools-py3")
            .make_depend("cmake")
            .make_depend("python-catkin_pkg")
            .make_depend("python-empy")
            .make_depend("python");
        raur.pkg("ros-melodic-cmake-modules")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-compressed-depth-image-transport")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-compressed-image-transport")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-control-msgs")
            .depend("ros-melodic-trajectory-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-trajectory-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-control-toolbox")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-control-msgs")
            .depend("ros-melodic-realtime-tools")
            .depend("ros-melodic-cmake-modules")
            .depend("tinyxml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-control-msgs")
            .make_depend("ros-melodic-realtime-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("tinyxml");
        raur.pkg("ros-melodic-controller-interface")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-hardware-interface")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-controller-manager")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-controller-manager-msgs")
            .depend("ros-melodic-hardware-interface")
            .depend("ros-melodic-controller-interface")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-controller-manager-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-controller-interface");
        raur.pkg("ros-melodic-controller-manager-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-std-msgs");
        raur.pkg("ros-melodic-cv-bridge")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("boost")
            .depend("python")
            .depend("python-numpy")
            .depend("opencv3-opt")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost")
            .make_depend("python")
            .make_depend("python-numpy")
            .make_depend("opencv3-opt");
        raur.pkg("ros-melodic-depth-image-proc")
            .depend("ros-melodic-image-geometry")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-eigen-conversions")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-image-transport")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-image-geometry")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-stereo-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-eigen-conversions")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport")
            .make_depend("boost");
        raur.pkg("ros-melodic-desktop")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-common-tutorials")
            .depend("ros-melodic-urdf-tutorial")
            .depend("ros-melodic-geometry-tutorials")
            .depend("ros-melodic-visualization-tutorials")
            .depend("ros-melodic-viz")
            .depend("ros-melodic-roslint")
            .depend("ros-melodic-robot")
            .depend("ros-melodic-ros-tutorials")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-desktop-full")
            .depend("ros-melodic-desktop")
            .depend("ros-melodic-simulators")
            .depend("ros-melodic-perception")
            .depend("ros-melodic-urdf-sim-tutorial")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-diagnostic-aggregator")
            .depend("ros-melodic-bondcpp")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-bondpy")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-pluginlib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-bondcpp")
            .make_depend("ros-melodic-xmlrpcpp")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-diagnostic-msgs")
            .make_depend("ros-melodic-bondpy")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-pluginlib");
        raur.pkg("ros-melodic-diagnostic-analysis")
            .depend("ros-melodic-roslib")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-diagnostic-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-diagnostic-msgs")
            .make_depend("ros-melodic-rosbag")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roslib");
        raur.pkg("ros-melodic-diagnostic-common-diagnostics")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-diagnostic-updater")
            .depend("python-psutil")
            .depend("hddtemp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-diagnostic-updater");
        raur.pkg("ros-melodic-diagnostic-updater")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-diagnostic-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-diagnostic-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp");
        raur.pkg("ros-melodic-diagnostics")
            .depend("ros-melodic-diagnostic-updater")
            .depend("ros-melodic-diagnostic-analysis")
            .depend("ros-melodic-diagnostic-common-diagnostics")
            .depend("ros-melodic-diagnostic-aggregator")
            .depend("ros-melodic-self-test")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-diff-drive-controller")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-realtime-tools")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-controller-interface")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-controller-manager")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-realtime-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-nav-msgs")
            .make_depend("ros-melodic-controller-interface")
            .make_depend("ros-melodic-tf");
        raur.pkg("ros-melodic-eigen-conversions")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-geometry-msgs")
            .depend("eigen3")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-executive-smach")
            .depend("ros-melodic-smach-msgs")
            .depend("ros-melodic-smach")
            .depend("ros-melodic-smach-ros")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-filters")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-roslib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-roslib");
        raur.pkg("ros-melodic-forward-command-controller")
            .depend("ros-melodic-controller-interface")
            .depend("ros-melodic-realtime-tools")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-hardware-interface")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-realtime-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-controller-interface")
            .make_depend("ros-melodic-std-msgs");
        raur.pkg("ros-melodic-gazebo-dev")
            .depend("gazebo")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-gazebo-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-trajectory-msgs")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-trajectory-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-sensor-msgs");
        raur.pkg("ros-melodic-gazebo-plugins")
            .depend("ros-melodic-diagnostic-updater")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-gazebo-msgs")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-trajectory-msgs")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-camera-info-manager")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-polled-camera")
            .depend("ros-melodic-image-transport")
            .depend("ros-melodic-gazebo-dev")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-diagnostic-updater")
            .make_depend("ros-melodic-rosgraph-msgs")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-gazebo-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-gazebo-dev")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-trajectory-msgs")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-nav-msgs")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-camera-info-manager")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-polled-camera")
            .make_depend("ros-melodic-image-transport")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-rospy");
        raur.pkg("ros-melodic-gazebo-ros")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-gazebo-msgs")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-roslib")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-gazebo-dev")
            .depend("tinyxml")
            .depend("python")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-gazebo-msgs")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roslib")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-rosgraph-msgs")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-gazebo-dev")
            .make_depend("tinyxml");
        raur.pkg("ros-melodic-gazebo-ros-control")
            .depend("ros-melodic-joint-limits-interface")
            .depend("ros-melodic-controller-manager")
            .depend("ros-melodic-hardware-interface")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-gazebo-ros")
            .depend("ros-melodic-control-toolbox")
            .depend("ros-melodic-transmission-interface")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-joint-limits-interface")
            .make_depend("ros-melodic-controller-manager")
            .make_depend("ros-melodic-transmission-interface")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-control-toolbox")
            .make_depend("ros-melodic-gazebo-dev");
        raur.pkg("ros-melodic-gazebo-ros-pkgs")
            .depend("ros-melodic-gazebo-msgs")
            .depend("ros-melodic-gazebo-ros")
            .depend("ros-melodic-gazebo-plugins")
            .depend("ros-melodic-gazebo-dev")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-geometry")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-kdl-conversions")
            .depend("ros-melodic-tf-conversions")
            .depend("ros-melodic-eigen-conversions")
            .depend("ros-melodic-tf")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-hardware-interface")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-image-common")
            .depend("ros-melodic-polled-camera")
            .depend("ros-melodic-camera-calibration-parsers")
            .depend("ros-melodic-camera-info-manager")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-image-geometry")
            .depend("ros-melodic-sensor-msgs")
            .depend("opencv")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("opencv");
        raur.pkg("ros-melodic-image-pipeline")
            .depend("ros-melodic-image-rotate")
            .depend("ros-melodic-stereo-image-proc")
            .depend("ros-melodic-depth-image-proc")
            .depend("ros-melodic-image-view")
            .depend("ros-melodic-image-proc")
            .depend("ros-melodic-image-publisher")
            .depend("ros-melodic-camera-calibration")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-image-proc")
            .depend("ros-melodic-image-geometry")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-nodelet-topic-tools")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-image-geometry")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-nodelet-topic-tools")
            .make_depend("ros-melodic-image-transport")
            .make_depend("boost");
        raur.pkg("ros-melodic-image-publisher")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-camera-info-manager")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-camera-info-manager")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-image-rotate")
            .depend("ros-melodic-tf2-geometry-msgs")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2-geometry-msgs")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-image-transport-plugins")
            .depend("ros-melodic-compressed-depth-image-transport")
            .depend("ros-melodic-compressed-image-transport")
            .depend("ros-melodic-theora-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-image-view")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-camera-calibration-parsers")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-image-transport")
            .depend("gtk2")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-stereo-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-camera-calibration-parsers")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport")
            .make_depend("gtk2");
        raur.pkg("ros-melodic-joint-limits-interface")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-hardware-interface")
            .depend("urdfdom")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-catkin")
            .make_depend("urdfdom");
        raur.pkg("ros-melodic-joint-state-controller")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-realtime-tools")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-hardware-interface")
            .depend("ros-melodic-controller-interface")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-realtime-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-controller-interface");
        raur.pkg("ros-melodic-kdl-conversions")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-kdl-parser-py")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-python-orocos-kdl")
            .depend("ros-melodic-urdfdom-py")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-catkin")
            .make_depend("python-catkin_pkg");
        raur.pkg("ros-melodic-laser-assembler")
            .depend("ros-melodic-filters")
            .depend("ros-melodic-laser-geometry")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-pluginlib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-filters")
            .make_depend("ros-melodic-laser-geometry")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-pluginlib");
        raur.pkg("ros-melodic-laser-filters")
            .depend("ros-melodic-filters")
            .depend("ros-melodic-laser-geometry")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-pluginlib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-filters")
            .make_depend("ros-melodic-laser-geometry")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-pluginlib");
        raur.pkg("ros-melodic-laser-pipeline")
            .depend("ros-melodic-laser-geometry")
            .depend("ros-melodic-laser-assembler")
            .depend("ros-melodic-laser-filters")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-pcl-conversions")
            .depend("ros-melodic-pcl-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("eigen3")
            .depend("pcl")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-pcl-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-std-msgs");
        raur.pkg("ros-melodic-pcl-ros")
            .depend("ros-melodic-pcl-conversions")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-tf2-eigen")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-nodelet-topic-tools")
            .depend("ros-melodic-pcl-msgs")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-tf")
            .depend("eigen3")
            .depend("pcl")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-pcl-conversions")
            .make_depend("ros-melodic-nodelet-topic-tools")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-tf2-eigen")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-roslib")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-rosbag")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-pcl-msgs")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("eigen3")
            .make_depend("pcl");
        raur.pkg("ros-melodic-perception")
            .depend("ros-melodic-vision-opencv")
            .depend("ros-melodic-ros-base")
            .depend("ros-melodic-perception-pcl")
            .depend("ros-melodic-laser-pipeline")
            .depend("ros-melodic-image-transport-plugins")
            .depend("ros-melodic-image-pipeline")
            .depend("ros-melodic-image-common")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-perception-pcl")
            .depend("ros-melodic-pcl-conversions")
            .depend("ros-melodic-pcl-ros")
            .depend("ros-melodic-pcl-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-polled-camera")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-position-controllers")
            .depend("ros-melodic-controller-interface")
            .depend("ros-melodic-forward-command-controller")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-controller-interface")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-forward-command-controller");
        raur.pkg("ros-melodic-python-orocos-kdl")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-catkin")
            .depend("python-sip")
            .depend("sip")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("python-sip")
            .make_depend("sip");
        raur.pkg("ros-melodic-realtime-tools")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rospy");
        raur.pkg("ros-melodic-robot")
            .depend("ros-melodic-filters")
            .depend("ros-melodic-ros-base")
            .depend("ros-melodic-joint-state-publisher")
            .depend("ros-melodic-executive-smach")
            .depend("ros-melodic-urdf-parser-plugin")
            .depend("ros-melodic-xacro")
            .depend("ros-melodic-diagnostics")
            .depend("ros-melodic-robot-state-publisher")
            .depend("ros-melodic-kdl-parser-py")
            .depend("ros-melodic-geometry")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-control-msgs")
            .depend("ros-melodic-kdl-parser")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-ros-environment")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-ros-tutorials")
            .depend("ros-melodic-roscpp-tutorials")
            .depend("ros-melodic-rospy-tutorials")
            .depend("ros-melodic-turtlesim")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roscpp-tutorials")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-rostime");
        raur.pkg("ros-melodic-roslang")
            .depend("ros-melodic-genmsg")
            .depend("ros-melodic-catkin")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roslib")
            .depend("ros-melodic-ros-environment")
            .depend("ros-melodic-rospack")
            .depend("ros-melodic-catkin")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rospack")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost");
        raur.pkg("ros-melodic-roslint")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rospack")
            .depend("ros-melodic-ros-environment")
            .depend("tinyxml2")
            .depend("python-rosdep")
            .depend("python-catkin_pkg")
            .depend("pkg-config")
            .depend("boost")
            .depend("python")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("tinyxml2")
            .make_depend("gtest")
            .make_depend("pkg-config")
            .make_depend("boost")
            .make_depend("python");
        raur.pkg("ros-melodic-rospy-tutorials")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rostest");
        raur.pkg("ros-melodic-rosunit")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-common-plugins")
            .depend("ros-melodic-rqt-bag-plugins")
            .depend("ros-melodic-rqt-launch")
            .depend("ros-melodic-rqt-action")
            .depend("ros-melodic-rqt-msg")
            .depend("ros-melodic-rqt-logger-level")
            .depend("ros-melodic-rqt-top")
            .depend("ros-melodic-rqt-service-caller")
            .depend("ros-melodic-rqt-shell")
            .depend("ros-melodic-rqt-graph")
            .depend("ros-melodic-rqt-topic")
            .depend("ros-melodic-rqt-web")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-bag")
            .depend("ros-melodic-rqt-plot")
            .depend("ros-melodic-rqt-publisher")
            .depend("ros-melodic-rqt-console")
            .depend("ros-melodic-rqt-srv")
            .depend("ros-melodic-rqt-dep")
            .depend("ros-melodic-rqt-image-view")
            .depend("ros-melodic-rqt-py-console")
            .depend("ros-melodic-rqt-reconfigure")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-dep")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rqt-graph")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-qt-dotgraph")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-image-view")
            .depend("ros-melodic-rqt-gui-cpp")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rqt-gui-cpp")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-rqt-gui")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-rqt-publisher")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-rosmsg")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-py-console")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-reconfigure")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-rqt-console")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-srv")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rosmsg")
            .depend("ros-melodic-rqt-msg")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-self-test")
            .depend("ros-melodic-diagnostic-updater")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-diagnostic-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-diagnostic-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-diagnostic-updater");
        raur.pkg("ros-melodic-simulators")
            .depend("ros-melodic-rqt-common-plugins")
            .depend("ros-melodic-robot")
            .depend("ros-melodic-gazebo-ros-pkgs")
            .depend("ros-melodic-rqt-robot-plugins")
            .depend("ros-melodic-stage-ros")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-smach")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-smach-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-smach-ros")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-smach-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-actionlib")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-smach")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rostest");
        raur.pkg("ros-melodic-stage")
            .depend("ros-melodic-catkin")
            .depend("libjpeg-turbo")
            .depend("mesa")
            .depend("fltk")
            .depend("gtk2")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("fltk")
            .make_depend("libjpeg-turbo")
            .make_depend("gtk2")
            .make_depend("libtool")
            .make_depend("mesa")
            .make_depend("pkg-config");
        raur.pkg("ros-melodic-stage-ros")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-stage")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-sensor-msgs")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-stage")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-nav-msgs")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("boost");
        raur.pkg("ros-melodic-stereo-image-proc")
            .depend("ros-melodic-image-geometry")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-stereo-msgs")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-image-proc")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-image-transport")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-image-geometry")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-stereo-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-image-proc")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-image-transport");
        raur.pkg("ros-melodic-tf-conversions")
            .depend("ros-melodic-kdl-conversions")
            .depend("ros-melodic-python-orocos-kdl")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-geometry-msgs")
            .depend("eigen3")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-kdl-conversions")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-tf2-eigen")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-geometry-msgs")
            .depend("eigen3")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-tf2-geometry-msgs")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-python-orocos-kdl")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-python-orocos-kdl")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-theora-image-transport")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-image-transport")
            .depend("libtheora")
            .depend("libogg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-rosbag")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cv-bridge")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-image-transport")
            .make_depend("libtheora")
            .make_depend("libogg");
        raur.pkg("ros-melodic-transmission-interface")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roscpp")
            .depend("tinyxml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-hardware-interface")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("tinyxml");
        raur.pkg("ros-melodic-urdf-parser-plugin")
            .depend("urdfdom-headers")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("urdfdom-headers");
        raur.pkg("ros-melodic-urdf-sim-tutorial")
            .depend("ros-melodic-position-controllers")
            .depend("ros-melodic-controller-manager")
            .depend("ros-melodic-joint-state-controller")
            .depend("ros-melodic-diff-drive-controller")
            .depend("ros-melodic-urdf-tutorial")
            .depend("ros-melodic-gazebo-ros-control")
            .depend("ros-melodic-rqt-robot-steering")
            .depend("ros-melodic-gazebo-ros")
            .depend("ros-melodic-xacro")
            .depend("ros-melodic-rviz")
            .depend("ros-melodic-robot-state-publisher")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-urdfdom-py")
            .depend("python-yaml")
            .depend("python-lxml")
            .depend("python")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("python");
        raur.pkg("ros-melodic-vision-opencv")
            .depend("ros-melodic-cv-bridge")
            .depend("ros-melodic-image-geometry")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-viz")
            .depend("ros-melodic-rqt-robot-plugins")
            .depend("ros-melodic-ros-base")
            .depend("ros-melodic-rviz")
            .depend("ros-melodic-rqt-common-plugins")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("console-bridge")
            .depend("gcc-libs")
            .make_depend("cmake");
        raur.pkg("log4cxx")
            .provide("log4cxx")
            .depend("apr-util")
            .depend("libxml2")
            .make_depend("autoconf")
            .make_depend("automake")
            .make_depend("libtool")
            .make_depend("patch")
            .make_depend("zip")
            .make_depend("gzip")
            .make_depend("sed");
        raur.pkg("ogre-1.9")
            .provide("ogre=1.9")
            .provide("ogre-docs=1.9")
            .depend("freeimage")
            .depend("freetype2")
            .depend("libxaw")
            .depend("libxrandr")
            .depend("openexr")
            .depend("nvidia-cg-toolkit")
            .depend("zziplib")
            .depend("sdl2")
            .depend("glu")
            .depend("tinyxml")
            .make_depend("cmake")
            .make_depend("doxygen")
            .make_depend("graphviz")
            .make_depend("ttf-dejavu")
            .make_depend("mesa")
            .make_depend("python")
            .make_depend("swig")
            .make_depend("systemd");
        raur.pkg("ros-melodic-actionlib")
            .depend("ros-melodic-rostest")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("wxpython")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-rospy")
            .make_depend("boost");
        raur.pkg("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-message-generation")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-actionlib-tutorials")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-actionlib")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-actionlib")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-roscpp");
        raur.pkg("ros-melodic-angles")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-bond")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-bond-core")
            .depend("ros-melodic-smclib")
            .depend("ros-melodic-bondpy")
            .depend("ros-melodic-bondcpp")
            .depend("ros-melodic-bond")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-bondcpp")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-smclib")
            .depend("ros-melodic-bond")
            .depend("boost")
            .depend("util-linux")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-smclib")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-bond")
            .make_depend("boost")
            .make_depend("util-linux");
        raur.pkg("ros-melodic-bondpy")
            .depend("ros-melodic-smclib")
            .depend("ros-melodic-rospy")
            .depend("util-linux")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-smclib")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-bond")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-class-loader")
            .depend("boost")
            .depend("console-bridge")
            .depend("poco")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost")
            .make_depend("console-bridge")
            .make_depend("poco");
        raur.pkg("ros-melodic-common-msgs")
            .depend("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-trajectory-msgs")
            .depend("ros-melodic-stereo-msgs")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-shape-msgs")
            .depend("ros-melodic-visualization-msgs")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-common-tutorials")
            .depend("ros-melodic-pluginlib-tutorials")
            .depend("ros-melodic-turtle-actionlib")
            .depend("ros-melodic-nodelet-tutorial-math")
            .depend("ros-melodic-actionlib-tutorials")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-cpp-common")
            .depend("boost")
            .depend("console-bridge")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost")
            .make_depend("console-bridge");
        raur.pkg("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-rosservice")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("boost");
        raur.pkg("ros-melodic-gencpp")
            .depend("ros-melodic-genmsg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-geneus")
            .depend("ros-melodic-genmsg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-genlisp")
            .depend("ros-melodic-genmsg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-genmsg")
            .depend("ros-melodic-catkin")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-gennodejs")
            .depend("ros-melodic-genmsg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-genpy")
            .depend("ros-melodic-genmsg")
            .depend("python-yaml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-geometry-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-geometry-tutorials")
            .depend("ros-melodic-turtle-tf")
            .depend("ros-melodic-turtle-tf2")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-gl-dependency")
            .depend("python-pyqt5")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-image-transport")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roslib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-roslib");
        raur.pkg("ros-melodic-interactive-marker-tutorials")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-visualization-msgs")
            .depend("ros-melodic-interactive-markers")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-interactive-markers")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-visualization-msgs");
        raur.pkg("ros-melodic-interactive-markers")
            .depend("ros-melodic-rostest")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-visualization-msgs")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-visualization-msgs")
            .make_depend("ros-melodic-rospy");
        raur.pkg("ros-melodic-joint-state-publisher")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-kdl-parser")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-rosconsole")
            .depend("tinyxml")
            .depend("tinyxml2")
            .depend("urdfdom-headers")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("tinyxml")
            .make_depend("tinyxml2")
            .make_depend("urdfdom-headers");
        raur.pkg("ros-melodic-laser-geometry")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-roscpp")
            .depend("boost")
            .depend("eigen3")
            .depend("python-numpy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("boost")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-librviz-tutorial")
            .depend("ros-melodic-rviz")
            .depend("ros-melodic-roscpp")
            .depend("qt5-base")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rviz")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-map-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-nav-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-nav-msgs")
            .make_depend("ros-melodic-sensor-msgs");
        raur.pkg("ros-melodic-media-export")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-message-filters")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-rosunit")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("boost");
        raur.pkg("ros-melodic-message-generation")
            .depend("ros-melodic-geneus")
            .depend("ros-melodic-gencpp")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-gennodejs")
            .depend("ros-melodic-genlisp")
            .depend("ros-melodic-genmsg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-message-runtime")
            .depend("ros-melodic-roscpp-traits")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-rostime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-mk")
            .depend("ros-melodic-rosbuild")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-nav-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-nodelet")
            .depend("ros-melodic-bondcpp")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-pluginlib")
            .depend("boost")
            .depend("util-linux")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-bondcpp")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("boost")
            .make_depend("util-linux");
        raur.pkg("ros-melodic-nodelet-core")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-nodelet-topic-tools")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-nodelet-topic-tools")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-pluginlib")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-dynamic-reconfigure")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost");
        raur.pkg("ros-melodic-nodelet-tutorial-math")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-orocos-kdl")
            .depend("ros-melodic-catkin")
            .depend("eigen3")
            .depend("pkg-config")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-pluginlib")
            .depend("ros-melodic-class-loader")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roslib")
            .depend("boost")
            .depend("tinyxml2")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-class-loader")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roslib")
            .make_depend("boost")
            .make_depend("tinyxml2");
        raur.pkg("ros-melodic-pluginlib-tutorials")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-python-qt-binding")
            .depend("python-pyqt5")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosbuild")
            .make_depend("ros-melodic-catkin")
            .make_depend("python-pyqt5")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-qt-dotgraph")
            .depend("ros-melodic-python-qt-binding")
            .depend("python-pydot")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-qt-gui")
            .depend("ros-melodic-python-qt-binding")
            .depend("python-rospkg")
            .depend("tango-icon-theme")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("python-pyqt5")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-qt-gui-cpp")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-pluginlib")
            .depend("tinyxml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-python-qt-binding")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("tinyxml")
            .make_depend("qt5-base")
            .make_depend("pkg-config");
        raur.pkg("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-python-qt-binding")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-qwt-dependency")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-resource-retriever")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roslib")
            .depend("boost")
            .depend("python-rospkg")
            .depend("curl")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roslib")
            .make_depend("boost")
            .make_depend("curl");
        raur.pkg("ros-melodic-robot-state-publisher")
            .depend("ros-melodic-tf2-kdl")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-catkin")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-kdl-parser")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-sensor-msgs")
            .depend("eigen3")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2-kdl")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-kdl-parser")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("eigen3")
            .make_depend("urdfdom-headers");
        raur.pkg("ros-melodic-ros")
            .depend("ros-melodic-rosmake")
            .depend("ros-melodic-rosboost-cfg")
            .depend("ros-melodic-rosbuild")
            .depend("ros-melodic-rosclean")
            .depend("ros-melodic-rosbash")
            .depend("ros-melodic-catkin")
            .depend("ros-melodic-rosunit")
            .depend("ros-melodic-mk")
            .depend("ros-melodic-roscreate")
            .depend("ros-melodic-roslang")
            .depend("ros-melodic-roslib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-ros-base")
            .depend("ros-melodic-bond-core")
            .depend("ros-melodic-nodelet-core")
            .depend("ros-melodic-dynamic-reconfigure")
            .depend("ros-melodic-ros-core")
            .depend("ros-melodic-actionlib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-ros-comm")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-rosmsg")
            .depend("ros-melodic-rosout")
            .depend("ros-melodic-rosparam")
            .depend("ros-melodic-topic-tools")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-ros")
            .depend("ros-melodic-roslisp")
            .depend("ros-melodic-roswtf")
            .depend("ros-melodic-rosmaster")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-rostest")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-roslaunch")
            .depend("ros-melodic-rosservice")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-ros-core")
            .depend("ros-melodic-geneus")
            .depend("ros-melodic-roscpp-core")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-gencpp")
            .depend("ros-melodic-rosconsole-bridge")
            .depend("ros-melodic-genmsg")
            .depend("ros-melodic-rosbag-migration-rule")
            .depend("ros-melodic-genlisp")
            .depend("ros-melodic-message-generation")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-ros")
            .depend("ros-melodic-roslisp")
            .depend("ros-melodic-class-loader")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-cmake-modules")
            .depend("ros-melodic-rospack")
            .depend("ros-melodic-catkin")
            .depend("ros-melodic-common-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-gennodejs")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-ros-comm")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-pluginlib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosbag")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-rosbag-storage")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-topic-tools")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-genmsg")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .depend("boost")
            .depend("python-gnupg")
            .depend("python-crypto")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-xmlrpcpp")
            .make_depend("ros-melodic-rosbag-storage")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-topic-tools")
            .make_depend("python-pillow")
            .make_depend("boost");
        raur.pkg("ros-melodic-rosbag-migration-rule")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosbag-storage")
            .depend("ros-melodic-roscpp-traits")
            .depend("ros-melodic-roslz4")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-pluginlib")
            .depend("console-bridge")
            .depend("gpgme")
            .depend("openssl")
            .depend("boost")
            .depend("bzip2")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-roscpp-traits")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roslz4")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("console-bridge")
            .make_depend("gpgme")
            .make_depend("openssl")
            .make_depend("boost")
            .make_depend("bzip2");
        raur.pkg("ros-melodic-rosbash")
            .depend("ros-melodic-catkin")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosboost-cfg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosbuild")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-message-generation")
            .depend("ros-melodic-catkin")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("pkg-config");
        raur.pkg("ros-melodic-rosclean")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosconsole")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-rosbuild")
            .depend("log4cxx")
            .depend("apr")
            .depend("apr-util")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-rosunit")
            .make_depend("ros-melodic-catkin")
            .make_depend("log4cxx")
            .make_depend("apr")
            .make_depend("boost")
            .make_depend("apr-util");
        raur.pkg("ros-melodic-rosconsole-bridge")
            .depend("ros-melodic-rosconsole")
            .depend("console-bridge")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-catkin")
            .make_depend("console-bridge");
        raur.pkg("ros-melodic-roscpp")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-roscpp-traits")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-xmlrpcpp")
            .make_depend("ros-melodic-roscpp-traits")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosgraph-msgs")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-roslang")
            .make_depend("pkg-config");
        raur.pkg("ros-melodic-roscpp-core")
            .depend("ros-melodic-roscpp-traits")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-roscpp-serialization")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-roscpp-traits")
            .depend("ros-melodic-cpp-common")
            .depend("ros-melodic-rostime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-roscpp-traits")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roscpp-traits")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-cpp-common")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roscreate")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosgraph")
            .depend("python-yaml")
            .depend("python-netifaces")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roslaunch")
            .depend("ros-melodic-rosout")
            .depend("ros-melodic-rosparam")
            .depend("ros-melodic-rosclean")
            .depend("ros-melodic-rosunit")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-rosmaster")
            .depend("ros-melodic-roslib")
            .depend("python-yaml")
            .depend("python-paramiko")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roslisp")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-roslang")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-rospack")
            .depend("sbcl")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-roslz4")
            .depend("lz4")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("lz4");
        raur.pkg("ros-melodic-rosmake")
            .depend("ros-melodic-catkin")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosmaster")
            .depend("ros-melodic-rosgraph")
            .depend("python-defusedxml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosmsg")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-catkin")
            .depend("ros-melodic-genmsg")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosnode")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-rosgraph")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rostest");
        raur.pkg("ros-melodic-rosout")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosgraph-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosparam")
            .depend("ros-melodic-rosgraph")
            .depend("python-yaml")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rospy")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-roslib")
            .depend("python-yaml")
            .depend("python-rospkg")
            .depend("python-numpy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rosservice")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosmsg")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rostest")
            .depend("ros-melodic-rosunit")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-rosmaster")
            .depend("ros-melodic-roslaunch")
            .depend("ros-melodic-rospy")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rosunit")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost");
        raur.pkg("ros-melodic-rostime")
            .depend("ros-melodic-cpp-common")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-catkin")
            .make_depend("boost");
        raur.pkg("ros-melodic-rostopic")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosbag")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rostest");
        raur.pkg("ros-melodic-roswtf")
            .depend("ros-melodic-rosbuild")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-roslaunch")
            .depend("ros-melodic-rosservice")
            .depend("ros-melodic-roslib")
            .depend("python-paramiko")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rostest");
        raur.pkg("ros-melodic-rqt-action")
            .depend("ros-melodic-rqt-msg")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-bag")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-bag-plugins")
            .depend("ros-melodic-rqt-plot")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-geometry-msgs")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-rqt-bag")
            .depend("ros-melodic-roslib")
            .depend("python-pillow")
            .depend("python-cairo")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-console")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rqt-logger-level")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-graph")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rosservice")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-rosgraph-msgs")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-qt-dotgraph")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-gui")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-catkin")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-qt-gui")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-gui-cpp")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-nodelet")
            .depend("ros-melodic-qt-gui-cpp")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-nodelet")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-qt-gui")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-qt-gui-cpp")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-qt-gui")
            .make_depend("ros-melodic-rqt-gui")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-launch")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rqt-console")
            .depend("ros-melodic-roslaunch")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rqt-py-common")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-logger-level")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rosservice")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-moveit")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rqt-topic")
            .depend("ros-melodic-rosnode")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-msg")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rosmsg")
            .depend("ros-melodic-rqt-console")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-nav-view")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-plot")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qwt-dependency")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("python-rospkg")
            .depend("python-matplotlib")
            .depend("python-numpy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-pose-view")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-gl-dependency")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .depend("python-opengl")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-py-common")
            .depend("ros-melodic-genpy")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-actionlib")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-genmsg")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-robot-dashboard")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-rqt-nav-view")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-rqt-console")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-rqt-robot-monitor")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-robot-monitor")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-rqt-bag")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rospy")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-robot-plugins")
            .depend("ros-melodic-rqt-tf-tree")
            .depend("ros-melodic-rqt-runtime-monitor")
            .depend("ros-melodic-rqt-nav-view")
            .depend("ros-melodic-rqt-pose-view")
            .depend("ros-melodic-rqt-robot-steering")
            .depend("ros-melodic-rqt-moveit")
            .depend("ros-melodic-rqt-robot-dashboard")
            .depend("ros-melodic-rqt-rviz")
            .depend("ros-melodic-rqt-robot-monitor")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-robot-steering")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-geometry-msgs")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-runtime-monitor")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-diagnostic-msgs")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-rviz")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rqt-gui-cpp")
            .depend("ros-melodic-rviz")
            .depend("ros-melodic-pluginlib")
            .depend("boost")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rviz")
            .make_depend("ros-melodic-rqt-gui-cpp")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rqt-gui")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("boost")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-rqt-service-caller")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rqt-py-common")
            .depend("ros-melodic-rosservice")
            .depend("ros-melodic-rqt-gui-py")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-shell")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui-py-common")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-tf-tree")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-rqt-graph")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-tf2-msgs")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-qt-dotgraph")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-top")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-rqt-gui-py")
            .depend("python-psutil")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-topic")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rostopic")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rqt-web")
            .depend("ros-melodic-rqt-gui-py")
            .depend("ros-melodic-qt-gui")
            .depend("ros-melodic-webkit-dependency")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-rqt-gui")
            .depend("ros-melodic-rospy")
            .depend("python-rospkg")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-rviz")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-rosbag")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-nav-msgs")
            .depend("ros-melodic-urdf")
            .depend("ros-melodic-python-qt-binding")
            .depend("ros-melodic-resource-retriever")
            .depend("ros-melodic-laser-geometry")
            .depend("ros-melodic-media-export")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-interactive-markers")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-roslib")
            .depend("ros-melodic-image-transport")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-visualization-msgs")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-map-msgs")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-geometry-msgs")
            .depend("yaml-cpp")
            .depend("eigen3")
            .depend("ogre-1.9")
            .depend("assimp")
            .depend("mesa")
            .depend("tinyxml2")
            .depend("urdfdom-headers")
            .depend("qt5-base")
            .depend("sip")
            .depend("python-sip")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-rosbag")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-nav-msgs")
            .make_depend("ros-melodic-urdf")
            .make_depend("ros-melodic-python-qt-binding")
            .make_depend("ros-melodic-resource-retriever")
            .make_depend("ros-melodic-laser-geometry")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-interactive-markers")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-roslib")
            .make_depend("ros-melodic-image-transport")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-visualization-msgs")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-map-msgs")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("yaml-cpp")
            .make_depend("eigen3")
            .make_depend("ogre-1.9")
            .make_depend("assimp")
            .make_depend("mesa")
            .make_depend("tinyxml2")
            .make_depend("urdfdom-headers")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-rviz-plugin-tutorials")
            .depend("ros-melodic-rviz")
            .depend("qt5-base")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rviz")
            .make_depend("ros-melodic-catkin")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-rviz-python-tutorial")
            .depend("ros-melodic-rviz")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rviz")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-sensor-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-shape-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-smclib")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-std-msgs")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-std-srvs")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-stereo-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-std-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-std-msgs");
        raur.pkg("ros-melodic-tf")
            .depend("ros-melodic-roswtf")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-sensor-msgs")
            .depend("ros-melodic-geometry-msgs")
            .depend("graphviz")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-sensor-msgs")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-tf2")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-tf2-msgs")
            .depend("ros-melodic-geometry-msgs")
            .depend("console-bridge")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-tf2-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("console-bridge");
        raur.pkg("ros-melodic-tf2-kdl")
            .depend("ros-melodic-orocos-kdl")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-tf2-ros")
            .depend("eigen3")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-orocos-kdl")
            .make_depend("eigen3");
        raur.pkg("ros-melodic-tf2-msgs")
            .depend("ros-melodic-message-generation")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-tf2-py")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-rospy")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-tf2-ros")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-tf2-py")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-actionlib")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-rosgraph")
            .depend("ros-melodic-message-filters")
            .depend("ros-melodic-tf2-msgs")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-xmlrpcpp")
            .make_depend("ros-melodic-tf2-py")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-actionlib")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-rosgraph")
            .make_depend("ros-melodic-message-filters")
            .make_depend("ros-melodic-tf2-msgs")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-topic-tools")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-message-runtime")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-xmlrpcpp")
            .make_depend("ros-melodic-rostest")
            .make_depend("ros-melodic-rosunit")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-rostime");
        raur.pkg("ros-melodic-trajectory-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosbag-migration-rule")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-turtle-actionlib")
            .depend("ros-melodic-angles")
            .depend("ros-melodic-turtlesim")
            .depend("ros-melodic-actionlib-msgs")
            .depend("ros-melodic-actionlib")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-angles")
            .make_depend("ros-melodic-turtlesim")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-actionlib-msgs")
            .make_depend("ros-melodic-actionlib")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-turtle-tf")
            .depend("ros-melodic-turtlesim")
            .depend("ros-melodic-tf")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-turtlesim")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-tf")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-turtle-tf2")
            .depend("ros-melodic-turtlesim")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-tf2-ros")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-tf2")
            .depend("ros-melodic-rospy")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-turtlesim")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-tf2-ros")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-tf2")
            .make_depend("ros-melodic-rospy")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-turtlesim")
            .depend("ros-melodic-std-srvs")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-roslib")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-rosconsole")
            .depend("ros-melodic-roscpp")
            .depend("ros-melodic-roscpp-serialization")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-geometry-msgs")
            .depend("qt5-base")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-std-srvs")
            .make_depend("ros-melodic-roslib")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-rosconsole")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-roscpp-serialization")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-geometry-msgs")
            .make_depend("qt5-base");
        raur.pkg("ros-melodic-urdf")
            .depend("ros-melodic-rosconsole-bridge")
            .depend("ros-melodic-pluginlib")
            .depend("ros-melodic-roscpp")
            .depend("tinyxml")
            .depend("tinyxml2")
            .depend("urdfdom")
            .depend("urdfdom-headers")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-urdf-parser-plugin")
            .make_depend("ros-melodic-cmake-modules")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-rosconsole-bridge")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-pluginlib")
            .make_depend("tinyxml")
            .make_depend("tinyxml2")
            .make_depend("urdfdom")
            .make_depend("urdfdom-headers");
        raur.pkg("ros-melodic-urdf-tutorial")
            .depend("ros-melodic-xacro")
            .depend("ros-melodic-joint-state-publisher")
            .depend("ros-melodic-rviz")
            .depend("ros-melodic-robot-state-publisher")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-roslaunch");
        raur.pkg("ros-melodic-visualization-marker-tutorials")
            .depend("ros-melodic-visualization-msgs")
            .depend("ros-melodic-roscpp")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-visualization-msgs")
            .make_depend("ros-melodic-roscpp")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-visualization-msgs")
            .depend("ros-melodic-message-runtime")
            .depend("ros-melodic-std-msgs")
            .depend("ros-melodic-geometry-msgs")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-message-generation")
            .make_depend("ros-melodic-std-msgs")
            .make_depend("ros-melodic-catkin")
            .make_depend("ros-melodic-geometry-msgs");
        raur.pkg("ros-melodic-visualization-tutorials")
            .depend("ros-melodic-visualization-marker-tutorials")
            .depend("ros-melodic-librviz-tutorial")
            .depend("ros-melodic-rviz-plugin-tutorials")
            .depend("ros-melodic-interactive-marker-tutorials")
            .depend("ros-melodic-rviz-python-tutorial")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-webkit-dependency")
            .depend("python-pyqt5")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-xacro")
            .depend("ros-melodic-roslaunch")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-roslint")
            .make_depend("ros-melodic-catkin");
        raur.pkg("ros-melodic-xmlrpcpp")
            .depend("ros-melodic-rostime")
            .depend("ros-melodic-cpp-common")
            .make_depend("cmake")
            .make_depend("ros-build-tools")
            .make_depend("ros-melodic-rostime")
            .make_depend("ros-melodic-cpp-common")
            .make_depend("ros-melodic-catkin");
        raur.pkg("tango-icon-theme")
            .make_depend("imagemagick")
            .make_depend("icon-naming-utils")
            .make_depend("intltool")
            .make_depend("librsvg");
        raur.pkg("urdfdom")
            .depend("tinyxml")
            .depend("console-bridge")
            .depend("urdfdom-headers")
            .make_depend("cmake");
        raur.pkg("urdfdom-headers").make_depend("cmake");

        raur.pkg("yay")
            .depend("pacman>=5.1")
            .depend("sudo")
            .depend("git")
            .make_depend("go");
        raur.pkg("yay-bin")
            .depend("pacman>=5.1")
            .depend("sudo")
            .depend("git")
            .make_depend("go")
            .provide("yay")
            .conflict("yay");
        raur.pkg("yay-git")
            .depend("pacman>=5.1")
            .depend("sudo")
            .depend("git")
            .make_depend("go")
            .provide("yay")
            .conflict("yay");

        raur.pkg("auracle-git")
            .provide("auracle")
            .depend("pacman")
            .depend("libarchive.so")
            .depend("libcurl.so")
            .depend("libsystemd.so")
            .make_depend("meson")
            .make_depend("git")
            .make_depend("perl");
        raur.pkg("pacaur")
            .depend("auracle-git")
            .depend("expac")
            .depend("sudo")
            .depend("git")
            .depend("jq")
            .make_depend("perl")
            .make_depend("git");

        raur.pkg("spotify")
            .depend("alsa-lib>=1.0.14")
            .depend("gconf")
            .depend("gtk2")
            .depend("glib2")
            .depend("nss")
            .depend("libsystemd")
            .depend("libxtst")
            .depend("libx11")
            .depend("libxss")
            .depend("desktop-file-utils")
            .depend("rtmpdump")
            .depend("openssl-1.0")
            .depend("libcurl-gnutls");

        raur.pkg("discord-canary")
            .version("0.0.96-1")
            .provide("discord")
            .make_depend("libc++")
            .depend("gtk3")
            .depend("libnotify")
            .depend("libxss")
            .depend("glibc")
            .depend("alsa-lib")
            .depend("nspr")
            .depend("nss")
            .depend("xdg-utils")
            .depend("libcups");
        raur.pkg("libc++")
            .version("8.0.1-1")
            .depend("libc++abi=8.0.1-1")
            .make_depend("clang")
            .make_depend("cmake")
            .make_depend("ninja")
            .make_depend("python")
            .make_depend("libunwind");
        raur.pkg("libc++abi")
            .version("8.0.1-1")
            .depend("gcc-libs")
            .make_depend("clang")
            .make_depend("cmake")
            .make_depend("ninja")
            .make_depend("python")
            .make_depend("libunwind");

        raur.pkg("pacman-git")
            .version("5.1.1.r160.gd37e6d40-2")
            .provide("pacman=5.1.1")
            .depend("archlinux-keyring")
            .depend("bash")
            .depend("curl")
            .depend("gpgme")
            .depend("libarchive")
            .depend("pacman-mirrorlist")
            .make_depend("git")
            .make_depend("asciidoc")
            .make_depend("meson")
            .conflict("pacman");

        raur.pkg("repo_version_test").depend("pacman-contrib>100");
        raur.pkg("satisfied_versioned_repo_dep")
            .depend("pacman>100");

        raur.pkg("version_equal").version("1-1");
        raur.pkg("version_newer").version("100-1");
        raur.pkg("version_older").version("0-1");

        raur.pkg("xterm");

        raur
    }

    fn _init_logger() {
        let _ = TermLogger::init(
            LevelFilter::Trace,
            ConfigBuilder::new()
                .add_filter_allow_str("aur_depends")
                .build(),
            TerminalMode::Stderr,
        );
    }

    fn alpm() -> Alpm {
        //let handle = Alpm::new("/", "/var/lib/pacman/").unwrap();
        let handle = Alpm::new("/", "tests/db").unwrap();
        handle.register_syncdb("core", SigLevel::NONE).unwrap();
        handle.register_syncdb("extra", SigLevel::NONE).unwrap();
        handle.register_syncdb("community", SigLevel::NONE).unwrap();
        handle.register_syncdb("multilib", SigLevel::NONE).unwrap();
        handle
    }

    fn resolve(pkgs: &[&str], flags: Flags) -> TestActions {
        let raur = raur();
        let alpm = alpm();
        let mut cache = HashSet::new();

        let handle = Resolver::new(&alpm, &mut cache, &raur, flags).provider_callback(|pkgs| {
            debug!("provider choice: {:?}", pkgs);
            if let Some(i) = pkgs.iter().position(|pkg| *pkg == "yay-bin") {
                i
            } else {
                0
            }
        });

        let actions = handle.resolve_targets(pkgs).unwrap();

        let mut build = actions
            .build
            .iter()
            .flat_map(|b| &b.pkgs)
            .map(|p| p.pkg.name.to_string())
            .collect::<Vec<_>>();

        let mut install = actions
            .install
            .iter()
            .map(|b| b.pkg.name().to_string())
            .collect::<Vec<_>>();

        build.sort();
        install.sort();

        let make = actions.install.iter().filter(|i| i.make).count()
            + actions
                .build
                .iter()
                .flat_map(|b| &b.pkgs)
                .filter(|i| i.make)
                .count();

        TestActions {
            install,
            build,
            missing: actions
                .missing
                .into_iter()
                .map(|m| m.stack.into_iter().chain(Some(m.dep)).collect())
                .collect(),
            make,
            duplicates: actions.duplicates,
        }
    }

    #[test]
    fn test_yay() {
        let TestActions { install, build, .. } = resolve(&["yay"], Flags::new());

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
    }

    #[test]
    fn test_yay_needed() {
        let TestActions { install, build, .. } = resolve(&["yay"], Flags::new() | Flags::NEEDED);

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
    }

    #[test]
    fn test_yay_no_deps() {
        let TestActions { install, build, .. } = resolve(&["yay"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_aur_yay_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["aur/yay"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, vec!["yay-bin"]);
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_core_yay_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_core_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/glibc"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, vec!["glibc"]);
    }

    #[test]
    fn test_aur_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_extra_glibc_no_deps() {
        let TestActions { install, build, .. } =
            resolve(&["core/yay"], Flags::new() | Flags::NO_DEPS);

        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_yay_no_provides() {
        let TestActions { install, build, .. } =
            resolve(&["yay"], Flags::new() & !Flags::TARGET_PROVIDES);

        assert_eq!(build, vec!["yay"]);
        assert_eq!(
            install,
            vec!["git", "go", "perl-error", "perl-mailtools", "perl-timedate"]
        );
    }

    #[test]
    fn test_make_only() {
        let TestActions { make, .. } = resolve(
            &["ros-melodic-desktop-full"],
            Flags::new() & !Flags::TARGET_PROVIDES & !Flags::MISSING_PROVIDES,
        );
        assert_eq!(make, 41);
    }

    #[test]
    fn test_cache_only() {
        let raur = raur();
        let alpm = alpm();
        let mut cache = HashSet::new();

        let mut handle = Resolver::new(&alpm, &mut cache, &raur, Flags::new());
        handle
            .cache_aur_pkgs_recursive(&["ros-melodic-desktop-full"], true)
            .unwrap();
    }

    #[test]
    fn test_pacaur() {
        let TestActions { install, build, .. } = resolve(&["pacaur"], Flags::new());
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

    #[test]
    fn test_pacaur_needed() {
        let TestActions { install, build, .. } = resolve(&["pacaur"], Flags::new() | Flags::NEEDED);
        assert_eq!(build, Vec::<String>::new());
        assert_eq!(install, Vec::<String>::new());
    }

    #[test]
    fn test_many() {
        let TestActions { install, build, .. } = resolve(
            &["yay", "pacaur", "pacman", "glibc", "0ad", "spotify"],
            Flags::new() | Flags::NO_DEPS,
        );

        assert_eq!(build, vec!["pacaur", "spotify", "yay-bin"]);
        assert_eq!(install, vec!["0ad", "glibc", "pacman"]);
    }

    #[test]
    fn test_many_needed() {
        let TestActions { install, build, .. } = resolve(
            &["yay", "pacaur", "pacman", "glibc", "0ad", "spotify"],
            Flags::new() | Flags::NO_DEPS | Flags::NEEDED,
        );

        assert_eq!(build, vec!["spotify", "yay-bin"]);
        assert_eq!(install, vec!["0ad", "glibc"]);
    }

    #[test]
    fn test_a() {
        let TestActions { missing, .. } = resolve(&["a"], Flags::new());

        assert_eq!(missing, vec![vec!["a", "b>1"]]);
    }

    #[test]
    fn test_a_no_ver() {
        let TestActions { build, .. } = resolve(&["a"], Flags::new() | Flags::NO_DEP_VERSION);

        assert_eq!(build, vec!["a", "b"]);
    }

    #[test]
    fn test_discord() {
        let TestActions {
            make,
            install,
            build,
            ..
        } = resolve(&["discord-canary"], Flags::new());

        assert_eq!(build.len(), 3);
        assert_eq!(install.len(), 89 + 13);
        assert_eq!(make, 11);
    }

    #[test]
    fn test_aur_only() {
        let TestActions { build, install, .. } = resolve(
            &["xterm", "yay"],
            Flags::new() | Flags::NO_DEPS | Flags::AUR_ONLY,
        );
        assert_eq!(build, vec!["xterm", "yay-bin"]);
        assert_eq!(install, Vec::<String>::new());

        let TestActions { install, .. } =
            resolve(&["pacman"], Flags::new() | Flags::NO_DEPS | Flags::AUR_ONLY);
        assert_eq!(install, Vec::<String>::new());
        //1assert_eq!(build, vec!["pacman-git"]);
    }

    #[test]
    fn test_repo_only() {
        let TestActions { build, install, .. } = resolve(
            &["xterm", "yay"],
            Flags::new() | Flags::NO_DEPS | Flags::REPO_ONLY,
        );
        assert_eq!(install, vec!["xterm"]);
        assert_eq!(build, Vec::<String>::new());

        let TestActions { install, build, .. } = resolve(
            &["pacman"],
            Flags::new() | Flags::NO_DEPS | Flags::REPO_ONLY,
        );
        assert_eq!(install, vec!["pacman"]);
        assert_eq!(build, Vec::<String>::new());
    }

    #[test]
    fn test_dups() {
        let TestActions { duplicates, .. } = resolve(&["extra/xterm", "aur/xterm"], Flags::new());

        assert_eq!(duplicates.len(), 1);
    }

    #[test]
    fn test_inner_conflicts() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let handle = Resolver::new(&alpm, &mut cache, &raur, Flags::new());
        let actions = handle
            .resolve_targets(&["yay", "yay-git", "yay-bin"])
            .unwrap();

        let mut conflict1 = Conflict::new("yay-bin".into());
        conflict1.push("yay".into(), &Depend::new("yay"));
        conflict1.push("yay-git".into(), &Depend::new("yay"));
        let mut conflict2 = Conflict::new("yay-git".into());
        conflict2.push("yay".into(), &Depend::new("yay"));
        conflict2.push("yay-bin".into(), &Depend::new("yay"));

        assert_eq!(actions.inner_conflicts, vec![conflict1, conflict2]);
    }

    #[test]
    fn test_conflicts() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let handle = Resolver::new(&alpm, &mut cache, &raur, Flags::new());
        let actions = handle.resolve_targets(&["pacman-git"]).unwrap();

        let mut conflict = Conflict::new("pacman-git".into());
        conflict.push("pacman".into(), &Depend::new("pacman"));

        assert_eq!(actions.conflicts, vec![conflict]);
    }

    #[test]
    fn test_aur_updates() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let mut handle = Resolver::new(&alpm, &mut cache, &raur, Flags::new());
        let pkgs = handle.aur_updates().unwrap().updates;
        let pkgs = pkgs
            .iter()
            .map(|p| p.remote.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(pkgs, vec!["version_newer"]);
    }

    #[test]
    fn test_aur_updates_enable_downgrade() {
        let alpm = alpm();
        let raur = raur();
        let mut cache = HashSet::new();
        let mut handle = Resolver::new(
            &alpm,
            &mut cache,
            &raur,
            Flags::new() | Flags::ENABLE_DOWNGRADE,
        );
        let pkgs = handle.aur_updates().unwrap().updates;
        let pkgs = pkgs
            .iter()
            .map(|p| p.remote.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(pkgs, vec!["pacaur", "version_newer", "version_older"]);
    }

    #[test]
    fn test_repo_nover() {
        let TestActions { install, .. } = resolve(&["repo_version_test"], Flags::new());
        assert_eq!(install, Vec::<String>::new());

        let TestActions { install, .. } =
            resolve(&["repo_version_test"], Flags::new() | Flags::NO_DEP_VERSION);
        assert_eq!(install, vec!["pacman-contrib"]);
    }

    #[test]
    fn test_satisfied_versioned_repo_dep() {
        let TestActions { missing, .. } = resolve(&["satisfied_versioned_repo_dep"], Flags::new());
        assert_eq!(
            missing,
            vec![vec!["satisfied_versioned_repo_dep", "pacman>100"]]
        );

        let TestActions { missing, .. } = resolve(
            &["satisfied_versioned_repo_dep"],
            Flags::new() | Flags::NO_DEP_VERSION,
        );
        assert_eq!(missing, Vec::<Vec<String>>::new());
    }

    #[test]
    fn test_resolve_targets() {
        //init_logger();
        let raur = raur();
        //let raur = raur::Handle::default();
        let alpm = alpm();
        let mut cache = HashSet::new();
        let flags = Flags::new() & !Flags::TARGET_PROVIDES & !Flags::MISSING_PROVIDES;

        let handle = Resolver::new(&alpm, &mut cache, &raur, flags).provider_callback(|pkgs| {
            println!("provider choice: {:?}", pkgs);
            0
        });

        let actions = handle
            .resolve_targets(&["ros-melodic-desktop-full"])
            //.resolve_targets(&["yay", "yay-bin", "yay-git"])
            //.resolve_targets(&["yay", "pikaur", "pacman", "glibc", "0ad", "spotify"])
            //.resolve_targets(&["0ad"])
            //.resolve_targets(&["linux-pf"])
            //.resolve_targets(&["ros-melodic-desktop-full", "yay"])
            //.resolve_targets(&["ignition-common"])
            .unwrap();

        actions
            .build
            .iter()
            .flat_map(|b| &b.pkgs)
            .for_each(|p| println!("b {}", p.pkg.name));

        actions
            .install
            .iter()
            .for_each(|p| println!("i {}", p.pkg.name()));

        actions
            .missing
            .iter()
            .for_each(|m| println!("missing {:?}", m));

        actions.conflicts.iter().for_each(|c| {
            println!("c {}: ", c.pkg);
            c.conflicting
                .iter()
                .for_each(|c| println!("    {} ({:?})", c.pkg, c.conflict))
        });

        actions.inner_conflicts.iter().for_each(|c| {
            println!("c {}: ", c.pkg);
            c.conflicting
                .iter()
                .for_each(|c| println!("    {} ({:?})", c.pkg, c.conflict))
        });

        actions.unneeded.iter().for_each(|p| println!("u {}", p));

        actions.duplicates.iter().for_each(|p| println!("d {}", p));

        println!(
            "build: {}",
            actions.build.iter().fold(0, |t, b| t + b.pkgs.len())
        );

        println!("install: {}", actions.install.len());
    }
}
