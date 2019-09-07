use std::collections::HashMap;

use raur::{Error, Package, Raur, SearchBy};

pub struct MockRaur {
    pub pkgs: HashMap<String, Package>,
}

impl Raur for MockRaur {
    type Err = Error;

    fn search_by<S: AsRef<str>>(&self, query: S, _by: SearchBy) -> Result<Vec<Package>, Self::Err> {
        Ok(self
            .pkgs
            .values()
            .filter(|p| {
                p.name.contains(query.as_ref())
                    || p.description
                        .as_ref()
                        .map(|s| s.as_str())
                        .unwrap_or_else(|| "")
                        .contains(query.as_ref())
            })
            .cloned()
            .collect())
    }

    fn info<S: AsRef<str>>(&self, pkg_names: &[S]) -> Result<Vec<Package>, Self::Err> {
        let mut out = Vec::new();

        for name in pkg_names {
            if let Some(pkg) = self.pkgs.get(name.as_ref()) {
                out.push(pkg.clone());
            }
        }

        Ok(out)
    }
}

impl MockRaur {
    pub fn new() -> Self {
        MockRaur {
            pkgs: HashMap::new(),
        }
    }

    pub fn pkg<S: Into<String>>(&mut self, name: S) -> MockPackage {
        let name = name.into();
        let pkg = Package {
            id: 0,
            name: name.clone(),
            package_base_id: 0,
            package_base: "name".into(),
            version: "0".into(),
            description: None,
            url: None,
            num_votes: 0,
            popularity: 0.0,
            out_of_date: None,
            maintainer: None,
            first_submitted: 0,
            last_modified: 0,
            url_path: "".into(),
            groups: vec![],
            depends: vec![],
            make_depends: vec![],
            opt_depends: vec![],
            check_depends: vec![],
            conflicts: vec![],
            replaces: vec![],
            provides: vec![],
            license: vec![],
            keywords: vec![],
        };

        self.pkgs.insert(pkg.name.clone(), pkg);
        MockPackage(self.pkgs.get_mut(&name).unwrap())
    }
}

pub struct MockPackage<'a>(&'a mut Package);

impl<'a> MockPackage<'a> {
    pub fn depend<S: Into<String>>(self, s: S) -> Self {
        self.0.depends.push(s.into());
        self
    }

    pub fn make_depend<S: Into<String>>(self, s: S) -> Self {
        self.0.make_depends.push(s.into());
        self
    }

    pub fn provide<S: Into<String>>(self, s: S) -> Self {
        self.0.provides.push(s.into());
        self
    }

    pub fn conflict<S: Into<String>>(self, s: S) -> Self {
        self.0.conflicts.push(s.into());
        self
    }

    pub fn version<S: Into<String>>(self, s: S) -> Self {
        self.0.version = s.into();
        self
    }
}
