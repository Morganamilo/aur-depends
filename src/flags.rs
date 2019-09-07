#![allow(missing_docs)]

use bitflags::bitflags;

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
        const CALCULATE_MAKE = 1 << 7;
        /// Calculate which packages are only needed to build the packages.
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
