#[macro_use]
extern crate structopt;

use self::prelude::*;
use cargo::{
    core::{package_id::PackageId, SourceId},
    util::important_paths::find_root_manifest_for_wd,
};
use crev_lib::ProofStore;
use crev_lib::{self, local::Local};
use default::default;
use semver;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use structopt::StructOpt;

mod crates_io;
mod opts;
mod prelude;

use crev_data::proof;
use crev_lib::{TrustOrDistrust, TrustOrDistrust::*};

struct Repo {
    manifest_path: PathBuf,
    config: cargo::util::config::Config,
}

impl Repo {
    fn auto_open_cwd() -> Result<Self> {
        cargo::core::enable_nightly_features();
        let cwd = std::env::current_dir()?;
        let manifest_path = find_root_manifest_for_wd(&cwd)?;
        let mut config = cargo::util::config::Config::default()?;
        config.configure(0, None, &None, false, false, &None, &[])?;
        Ok(Repo {
            manifest_path,
            config,
        })
    }

    fn for_every_dependency_dir(
        &self,
        mut f: impl FnMut(&PackageId, &Path) -> Result<()>,
    ) -> Result<()> {
        let workspace = cargo::core::Workspace::new(&self.manifest_path, &self.config)?;
        let specs = cargo::ops::Packages::All.to_package_id_specs(&workspace)?;
        let (package_set, _resolve) = cargo::ops::resolve_ws_precisely(
            &workspace,
            None,
            &[],
            true,  // all_features
            false, // no_default_features
            &specs,
        )?;
        let source_id = SourceId::crates_io(&self.config)?;
        let map = cargo::sources::SourceConfigMap::new(&self.config)?;
        let mut source = map.load(&source_id)?;
        source.update()?;

        for pkg_id in package_set.package_ids() {
            let pkg = package_set.get(pkg_id)?;

            if !pkg.root().exists() {
                source.download(pkg_id)?;
            }

            f(&pkg_id, &pkg.root())?;
        }

        Ok(())
    }

    fn find_dependency_dir(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Result<(PathBuf, semver::Version)> {
        let mut ret = vec![];

        self.for_every_dependency_dir(|pkg_id, path| {
            if name == pkg_id.name().as_str()
                && (version.is_none() || version == Some(&pkg_id.version().to_string()))
            {
                ret.push((path.to_owned(), pkg_id.version().to_owned()));
            }
            Ok(())
        })?;

        match ret.len() {
            0 => bail!("Not found"),
            1 => Ok(ret[0].clone()),
            n => bail!("{} matches found", n),
        }
    }
}

fn cargo_ignore_list() -> HashSet<PathBuf> {
    let mut ignore_list = HashSet::new();
    ignore_list.insert(PathBuf::from(".cargo-ok"));
    ignore_list.insert(PathBuf::from("Cargo.lock"));
    ignore_list.insert(PathBuf::from("target"));
    ignore_list
}

fn review_crate(args: &opts::CrateSelectorNameRequired, trust: TrustOrDistrust) -> Result<()> {
    let repo = Repo::auto_open_cwd()?;
    let (pkg_dir, crate_version) = repo.find_dependency_dir(&args.name, args.version.as_deref())?;
    let local = Local::auto_open()?;

    // to protect from creating a digest from a crate in unclean state
    // we move the old directory, download a fresh one and double
    // check if the digest was the same
    let reviewed_pkg_dir = pkg_dir.with_extension("crev.reviewed");
    if reviewed_pkg_dir.is_dir() {
        std::fs::remove_dir_all(&reviewed_pkg_dir)?;
    }
    std::fs::rename(&pkg_dir, &reviewed_pkg_dir)?;
    let (pkg_dir_second, crate_version_second) =
        repo.find_dependency_dir(&args.name, args.version.as_deref())?;
    assert_eq!(pkg_dir, pkg_dir_second);
    assert_eq!(crate_version, crate_version_second);

    let digest_clean = crev_lib::get_recursive_digest_for_dir(&pkg_dir, &cargo_ignore_list())?;
    let digest_reviewed =
        crev_lib::get_recursive_digest_for_dir(&reviewed_pkg_dir, &cargo_ignore_list())?;

    if digest_clean != digest_reviewed {
        bail!(
            "The digest of the reviewed and freshly downloaded crate were different; {} != {}; {} != {}",
            digest_clean,
            digest_reviewed,
            pkg_dir.display(),
            reviewed_pkg_dir.display(),
        );
    }
    std::fs::remove_dir_all(&reviewed_pkg_dir)?;

    let passphrase = crev_common::read_passphrase()?;
    let id = local.read_current_unlocked_id(&passphrase)?;

    let review = proof::review::PackageBuilder::default()
        .from(id.id.to_owned())
        .package(proof::PackageInfo {
            id: None,
            source: PROJECT_SOURCE_CRATES_IO.to_owned(),
            name: args.name.clone(),
            version: crate_version.to_string(),
            digest: digest_clean.into_vec(),
            digest_type: proof::default_digest_type(),
            revision: "".into(),
            revision_type: proof::default_revision_type(),
        })
        .review(trust.to_review())
        .build()
        .map_err(|e| format_err!("{}", e))?;

    let review = crev_lib::util::edit_proof_content_iteractively(&review.into())?;

    let proof = review.sign_by(&id)?;

    local.insert(&proof)?;
    Ok(())
}
const PROJECT_SOURCE_CRATES_IO: &str = "https://crates.io";

fn find_reviews(
    crate_: &opts::CrateSelector,
    trust_params: &crev_lib::trustdb::TrustDistanceParams,
) -> Result<impl Iterator<Item = proof::review::Package>> {
    let local = crev_lib::Local::auto_open()?;
    let (db, _trust_set) = local.load_db(&trust_params)?;
    Ok(db.get_package_reviews_for_package(
        PROJECT_SOURCE_CRATES_IO,
        crate_.name.as_ref().map(|s| s.as_str()),
        crate_.version.as_ref().map(|s| s.as_str()),
    ))
}

fn list_reviews(crate_: &opts::CrateSelector) -> Result<()> {
    // TODO: take trust params?
    for review in find_reviews(crate_, &default())? {
        println!("{}", review);
    }

    Ok(())
}

fn tilda_home_path(home: &Option<PathBuf>, path: &Path) -> String {
    if let Some(home) = home {
        match path.strip_prefix(home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => path.display().to_string(),
        }
    } else {
        path.display().to_string()
    }
}

fn main() -> Result<()> {
    let opts = opts::Opts::from_args();
    let opts::MainCommand::Crev(command) = opts.command;
    match command {
        opts::Command::New(cmd) => match cmd {
            opts::New::Id(args) => {
                let res =
                    crev_lib::generate_id(args.url, args.github_username, args.use_https_push);
                if res.is_err() {
                    eprintln!("Visit https://github.com/dpc/crev/wiki/Proof-Repository for help.");
                }
                res?;
            }
        },
        opts::Command::Switch(cmd) => match cmd {
            opts::Switch::Id(args) => crev_lib::switch_id(&args.id)?,
        },
        opts::Command::Edit(cmd) => match cmd {
            opts::Edit::Readme => {
                let local = crev_lib::Local::auto_open()?;
                local.edit_readme()?;
            }
        },
        opts::Command::Verify(cmd) => match cmd {
            opts::Verify::Deps(args) => {
                let local = crev_lib::Local::auto_open()?;
                let (db, trust_set) = local.load_db(&args.trust_params.clone().into())?;

                let repo = Repo::auto_open_cwd()?;
                let ignore_list = cargo_ignore_list();
                let current_dir = std::env::current_dir()?;
                let cratesio = crates_io::Client::new(&local)?;
                let home_dir = dirs::home_dir();

                repo.for_every_dependency_dir(|pkg_id, path| {
                    if path.starts_with(&current_dir) {
                        // ignore local dependencies
                        return Ok(());
                    }

                    let pkg_name = pkg_id.name().as_str();
                    let pkg_version = pkg_id.version().to_string();

                    let digest = crev_lib::get_dir_digest(&path, &ignore_list)?;
                    let result = db.verify_digest(&digest, &trust_set);
                    let pkg_review_count =
                        db.get_package_review_count(PROJECT_SOURCE_CRATES_IO, Some(pkg_name), None);
                    let pkg_version_review_count = db.get_package_review_count(
                        PROJECT_SOURCE_CRATES_IO,
                        Some(pkg_name),
                        Some(&pkg_version),
                    );

                    let (version_downloads, total_downloads) = cratesio
                        .get_downloads_count(&pkg_name, &pkg_version)
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                        .unwrap_or_else(|e| {
                            eprintln!("Error: {}", e);
                            ("err".into(), "err".into())
                        });

                    if args.verbose {
                        println!(
                            "{:8} {:2} {:2} {:>7} {:>8} {} {:40}",
                            result,
                            pkg_version_review_count,
                            pkg_review_count,
                            version_downloads,
                            total_downloads,
                            digest,
                            tilda_home_path(&home_dir, &path)
                        );
                    } else {
                        println!(
                            "{:8} {:2} {:2} {:>7} {:>8} {:40}",
                            result,
                            pkg_version_review_count,
                            pkg_review_count,
                            version_downloads,
                            total_downloads,
                            tilda_home_path(&home_dir, &path)
                        );
                    }

                    Ok(())
                })?;
            }
        },
        opts::Command::Query(cmd) => match cmd {
            opts::Query::Id(cmd) => match cmd {
                opts::QueryId::Current => crev_lib::show_current_id()?,
                opts::QueryId::Own => crev_lib::list_own_ids()?,
                opts::QueryId::Trusted(args) => {
                    let local = crev_lib::Local::auto_open()?;
                    let (_db, trust_set) = local.load_db(&args.trust_params.into())?;
                    for id in &trust_set {
                        println!("{}", id);
                    }
                }
                opts::QueryId::All => {
                    let local = crev_lib::Local::auto_open()?;
                    let (db, _trust_set) = local.load_db(&default())?;

                    for id in &db.all_known_ids() {
                        println!("{}", id);
                    }
                }
            },
            opts::Query::Review(args) => list_reviews(&args.crate_)?,
        },
        opts::Command::Review(args) => {
            review_crate(&args, TrustOrDistrust::Trust)?;
        }
        opts::Command::Flag(args) => {
            review_crate(&args, TrustOrDistrust::Distrust)?;
        }
        opts::Command::Trust(args) => {
            let local = Local::auto_open()?;
            let passphrase = crev_common::read_passphrase()?;
            local.build_trust_proof(args.pub_ids, &passphrase, Trust)?;
        }
        opts::Command::Distrust(args) => {
            let local = Local::auto_open()?;
            let passphrase = crev_common::read_passphrase()?;
            local.build_trust_proof(args.pub_ids, &passphrase, Distrust)?;
        }
        opts::Command::Git(git) => {
            let local = Local::auto_open()?;
            let status = local.run_git(git.args)?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Command::Diff => {
            let local = Local::auto_open()?;
            let status = local.run_git(vec!["diff".into(), "HEAD".into()])?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Command::Commit => {
            let local = Local::auto_open()?;
            let status = local.run_git(vec!["commit".into(), "-a".into()])?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Command::Push => {
            let local = Local::auto_open()?;
            let status = local.run_git(vec!["push".into()])?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Command::Pull => {
            let local = Local::auto_open()?;
            let status = local.run_git(vec!["pull".into()])?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Command::Fetch(cmd) => match cmd {
            opts::Fetch::Trusted(params) => {
                let local = Local::auto_open()?;
                local.fetch_trusted(params.into())?;
            }
            opts::Fetch::Url(params) => {
                let local = Local::auto_open()?;
                local.fetch_url(&params.url)?;
            }
            opts::Fetch::All => {
                let local = Local::auto_open()?;
                local.fetch_all()?;
            }
        },
    }

    Ok(())
}
