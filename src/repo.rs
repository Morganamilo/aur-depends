use srcinfo::Srcinfo;

#[derive(Debug, Clone)]
/// A custom repo.
pub struct Repo {
    /// Name of the repo.
    pub name: String,
    /// Packages in the repo.
    pub pkgs: Vec<Srcinfo>,
}

impl Repo {
    /// Creates a new emptry repo with the given name.
    pub fn new(name: String) -> Self {
        Repo {
            name,
            pkgs: Vec::new(),
        }
    }
}
