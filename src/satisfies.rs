use alpm::{AsDep, Dep, Depend, Ver, Version};
use alpm_utils::depends;

pub trait Satisfies {
    fn satisfies_dep(&self, dep: &Dep, nover: bool) -> bool;
}

impl Satisfies for raur::Package {
    fn satisfies_dep(&self, dep: &Dep, nover: bool) -> bool {
        let provides = self.provides.iter().map(|p| Depend::new(p.as_str()));
        satisfies(
            dep,
            &self.name,
            Version::new(&*self.version),
            provides,
            nover,
        )
    }
}

impl<'a> Satisfies for alpm::Package<'a> {
    fn satisfies_dep(&self, dep: &Dep, nover: bool) -> bool {
        satisfies(
            dep,
            self.name(),
            self.version(),
            self.provides().iter(),
            nover,
        )
    }
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
