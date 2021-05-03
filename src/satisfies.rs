use alpm::{AsDep, Dep, Depend, Ver, Version};
use alpm_utils::depends;

pub fn satisfies_aur_pkg(dep: &Dep, pkg: &raur::Package, nover: bool) -> bool {
    let provides = pkg.provides.iter().map(|p| Depend::new(p.as_str()));
    satisfies(dep, &pkg.name, Version::new(&*pkg.version), provides, nover)
}

pub fn satisfies_repo_pkg(dep: &Dep, pkg: &alpm::Package, nover: bool) -> bool {
    satisfies(dep, pkg.name(), pkg.version(), pkg.provides().iter(), nover)
}

pub fn satisfies<D: AsDep, S: AsRef<str>, V: AsRef<Ver>>(
    dep: &Dep,
    name: S,
    version: V,
    provides: impl Iterator<Item = D>,
    nover: bool,
) -> bool {
    if nover {
        depends::satisfies_nover(dep, name, provides)
    } else {
        depends::satisfies(dep, name, version, provides)
    }
}

pub fn satisfies_provide(dep: &Dep, provide: &Dep, nover: bool) -> bool {
    if nover {
        depends::satisfies_provide_nover(dep, provide)
    } else {
        depends::satisfies_provide(dep, provide)
    }
}
