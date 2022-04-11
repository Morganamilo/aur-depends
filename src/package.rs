use raur::ArcPackage;
use srcinfo::{PackageBase, Package};

use crate::{Resolver, Flags};

pub enum AurOrCustomPackage {
    Aur(ArcPackage),
    Custom(srcinfo::PackageBase, srcinfo::Package),
}

impl<'a, 'b> Resolver<'a, 'b> {
    fn depends(&self, pkg: &AurOrCustomPackage) -> Vec<&str> {
        match &pkg {
            AurOrCustomPackage::Aur(pkg) => {
            let check = if self.flags.contains(Flags::CHECK_DEPENDS) {
                Some(&pkg.check_depends)
            } else {
                None
            };

            pkg
                .make_depends
                .iter()
                .chain(check.into_iter().flatten())
                .chain(&pkg.depends)
            },
            AurOrCustomPackage::Custom(base, pkg) => {
            let check = if self.flags.contains(Flags::CHECK_DEPENDS) {
                Some(&base.checkdepends)
            } else {
                None
            };

            base
                .makedepends
                .iter()
                .chain(check.into_iter().flatten())
                .chain(&pkg.depends)
            },
        }
    }
}
