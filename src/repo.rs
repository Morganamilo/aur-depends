use srcinfo::Srcinfo;

#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
    pub pkgs: Vec<Srcinfo>,
}
