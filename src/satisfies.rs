use alpm::{Depend, Ver, Version};
use alpm_utils::depends;

pub fn satisfies_aur_pkg(dep: &Depend, pkg: &raur::Package, nover: bool) -> bool {
    let provides = pkg.provides.iter().map(|p| Depend::new(p));
    satisfies(dep, &pkg.name, Version::new(&pkg.version), provides, nover)
}

pub fn satisfies_repo_pkg(dep: &Depend, pkg: &alpm::Package, nover: bool) -> bool {
    satisfies(dep, pkg.name(), pkg.version(), pkg.provides(), nover)
}

pub fn satisfies<'a, S: AsRef<str>, V: AsRef<Ver>>(
    dep: &Depend,
    name: S,
    version: V,
    provides: impl Iterator<Item = Depend<'a>>,
    nover: bool,
) -> bool {
    if nover {
        depends::satisfies_nover(dep, name, provides)
    } else {
        depends::satisfies(dep, name, version, provides)
    }
}
