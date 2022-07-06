use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwapOption;
use git2::{Oid, Repository, Signature};
use moka::future::Cache;
use time::OffsetDateTime;

pub type RepositoryMetadataList = BTreeMap<Option<String>, Vec<RepositoryMetadata>>;

#[derive(Clone)]
pub struct Git {
    commits: Cache<Oid, Arc<Commit>>,
    readme_cache: Cache<PathBuf, Arc<str>>,
    refs: Cache<PathBuf, Arc<Refs>>,
    repository_metadata: Arc<ArcSwapOption<RepositoryMetadataList>>,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            commits: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            readme_cache: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            refs: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .max_capacity(100)
                .build(),
            repository_metadata: Arc::new(ArcSwapOption::default()),
        }
    }
}

impl Git {
    pub async fn get_commit<'a>(&'a self, repo: PathBuf, commit: &str) -> Arc<Commit> {
        let commit = Oid::from_str(commit).unwrap();

        self.commits
            .get_with(commit, async {
                tokio::task::spawn_blocking(move || {
                    let repo = Repository::open_bare(repo).unwrap();
                    let commit = repo.find_commit(commit).unwrap();

                    Arc::new(Commit::from(commit))
                })
                .await
                .unwrap()
            })
            .await
    }

    pub async fn get_refs(&self, repo: PathBuf) -> Arc<Refs> {
        self.refs
            .get_with(repo.clone(), async {
                tokio::task::spawn_blocking(move || {
                    let repo = git2::Repository::open_bare(repo).unwrap();
                    let ref_iter = repo.references().unwrap();

                    let mut built_refs = Refs::default();

                    for ref_ in ref_iter {
                        let ref_ = ref_.unwrap();

                        if ref_.is_branch() {
                            let commit = ref_.peel_to_commit().unwrap();

                            built_refs.branch.push(Branch {
                                name: ref_.shorthand().unwrap().to_string(),
                                commit: commit.into(),
                            });
                        } else if ref_.is_tag() {
                            let tag = ref_.peel_to_tag().unwrap();

                            built_refs.tag.push(Tag {
                                name: ref_.shorthand().unwrap().to_string(),
                                tagger: tag.tagger().map(Into::into),
                            });
                        }
                    }

                    Arc::new(built_refs)
                })
                .await
                .unwrap()
            })
            .await
    }

    pub async fn get_readme(&self, repo: PathBuf) -> Arc<str> {
        self.readme_cache
            .get_with(repo.clone(), async {
                tokio::task::spawn_blocking(move || {
                    let repo = Repository::open_bare(repo).unwrap();
                    let head = repo.head().unwrap();
                    let commit = head.peel_to_commit().unwrap();
                    let tree = commit.tree().unwrap();

                    let object = tree
                        .get_name("README.md")
                        .unwrap()
                        .to_object(&repo)
                        .unwrap();
                    let blob = object.into_blob().unwrap();

                    Arc::from(String::from_utf8(blob.content().to_vec()).unwrap())
                })
                .await
                .unwrap()
            })
            .await
    }

    pub async fn get_latest_commit(&self, repo: PathBuf) -> Commit {
        tokio::task::spawn_blocking(move || {
            let repo = Repository::open_bare(repo).unwrap();
            let head = repo.head().unwrap();
            let commit = head.peel_to_commit().unwrap();

            Commit::from(commit)
        })
        .await
        .unwrap()
    }

    pub async fn fetch_repository_metadata(&self) -> Arc<RepositoryMetadataList> {
        if let Some(metadata) = self.repository_metadata.load().as_ref() {
            return Arc::clone(metadata);
        }

        let start = Path::new("../test-git").canonicalize().unwrap();

        let repos = tokio::task::spawn_blocking(move || {
            let mut repos: RepositoryMetadataList = RepositoryMetadataList::new();
            fetch_repository_metadata_impl(&start, &start, &mut repos);
            repos
        })
        .await
        .unwrap();

        let repos = Arc::new(repos);
        self.repository_metadata.store(Some(repos.clone()));

        repos
    }

    pub async fn get_commits(
        &self,
        repo: PathBuf,
        branch: Option<&str>,
        offset: usize,
    ) -> (Vec<Commit>, Option<usize>) {
        const AMOUNT: usize = 200;

        let ref_name = branch.map(|branch| format!("refs/heads/{}", branch));

        tokio::task::spawn_blocking(move || {
            let repo = Repository::open_bare(repo).unwrap();
            let mut revs = repo.revwalk().unwrap();

            if let Some(ref_name) = ref_name.as_deref() {
                revs.push_ref(ref_name).unwrap();
            } else {
                revs.push_head().unwrap();
            }

            let mut commits: Vec<Commit> = revs
                .skip(offset)
                .take(AMOUNT + 1)
                .map(|rev| {
                    let rev = rev.unwrap();
                    repo.find_commit(rev).unwrap().into()
                })
                .collect();

            // TODO: avoid having to take + 1 and popping the last commit off
            let next_offset = commits.pop().is_some().then(|| offset + commits.len());

            (commits, next_offset)
        })
        .await
        .unwrap()
    }
}

#[derive(Debug, Default)]
pub struct Refs {
    pub branch: Vec<Branch>,
    pub tag: Vec<Tag>,
}

#[derive(Debug)]
pub struct Branch {
    pub name: String,
    pub commit: Commit,
}

#[derive(Debug)]
pub struct Remote {
    pub name: String,
}

#[derive(Debug)]
pub struct Tag {
    pub name: String,
    pub tagger: Option<CommitUser>,
}

#[derive(Debug)]
pub struct RepositoryMetadata {
    pub name: String,
    pub description: Option<Cow<'static, str>>,
    pub owner: Option<String>,
    pub last_modified: Duration,
}

#[derive(Debug)]
pub struct CommitUser {
    name: String,
    email: String,
    email_md5: String,
    time: String,
}

impl From<Signature<'_>> for CommitUser {
    fn from(v: Signature<'_>) -> Self {
        CommitUser {
            name: v.name().unwrap().to_string(),
            email: v.email().unwrap().to_string(),
            email_md5: format!("{:x}", md5::compute(v.email_bytes())),
            time: OffsetDateTime::from_unix_timestamp(v.when().seconds())
                .unwrap()
                .to_string(),
        }
    }
}

impl CommitUser {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn email_md5(&self) -> &str {
        &self.email_md5
    }

    pub fn time(&self) -> &str {
        &self.time
    }
}

#[derive(Debug)]
pub struct Commit {
    author: CommitUser,
    committer: CommitUser,
    oid: String,
    tree: String,
    parents: Vec<String>,
    summary: String,
    body: String,
}

impl From<git2::Commit<'_>> for Commit {
    fn from(commit: git2::Commit<'_>) -> Self {
        Commit {
            author: commit.author().into(),
            committer: commit.committer().into(),
            oid: commit.id().to_string(),
            tree: commit.tree_id().to_string(),
            parents: commit.parent_ids().map(|v| v.to_string()).collect(),
            summary: commit.summary().unwrap().to_string(),
            body: commit.body().map(ToString::to_string).unwrap_or_default(),
        }
    }
}

impl Commit {
    pub fn author(&self) -> &CommitUser {
        &self.author
    }

    pub fn committer(&self) -> &CommitUser {
        &self.committer
    }

    pub fn oid(&self) -> &str {
        &self.oid
    }

    pub fn tree(&self) -> &str {
        &self.tree
    }

    pub fn parents(&self) -> impl Iterator<Item = &str> {
        self.parents.iter().map(String::as_str)
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

fn fetch_repository_metadata_impl(
    start: &Path,
    current: &Path,
    repos: &mut RepositoryMetadataList,
) {
    let dirs = std::fs::read_dir(current)
        .unwrap()
        .map(|v| v.unwrap().path())
        .filter(|path| path.is_dir());

    for dir in dirs {
        let repository = match Repository::open_bare(&dir) {
            Ok(v) => v,
            Err(_e) => {
                fetch_repository_metadata_impl(start, &dir, repos);
                continue;
            }
        };

        let repo_path = Some(
            current
                .strip_prefix(start)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
        .filter(|v| !v.is_empty());
        let repos = repos.entry(repo_path).or_default();

        let description = std::fs::read_to_string(dir.join("description"))
            .map(Cow::Owned)
            .ok();
        let last_modified = std::fs::metadata(&dir).unwrap().modified().unwrap();
        let owner = repository.config().unwrap().get_string("gitweb.owner").ok();

        repos.push(RepositoryMetadata {
            name: dir
                .components()
                .last()
                .unwrap()
                .as_os_str()
                .to_string_lossy()
                .into_owned(),
            description,
            owner,
            last_modified: (OffsetDateTime::now_utc() - OffsetDateTime::from(last_modified))
                .unsigned_abs(),
        });
    }
}
