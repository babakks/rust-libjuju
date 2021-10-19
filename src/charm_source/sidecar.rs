use std::collections::HashMap;
use std::env::current_dir;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::from_utf8;

use ex::fs::{read, File};
use serde_derive::{Deserialize, Serialize};
use serde_yaml::from_slice;
use tempfile::TempDir;
use zip::ZipArchive;

use crate::charm_url::CharmURL;
use crate::cmd;
use crate::error::JujuError;

/// Config option as defined in config.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "kebab-case")]
pub enum ConfigOption {
    /// String config option
    String {
        default: Option<String>,
        description: String,
    },

    /// Integer config option
    #[serde(rename = "int")]
    Integer { default: i64, description: String },

    /// Boolean config option
    Boolean { default: bool, description: String },
}

/// A charm's config.yaml file
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub options: HashMap<String, ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Container {
    /// Back oci-image resource
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum ResourceType {
    File,
    OciImage,
    Pypi,
    Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Resource {
    #[serde(rename = "type")]
    pub kind: ResourceType,
    pub description: String,
    pub upstream_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum RelationScope {
    Global,
    Container,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Interface {
    pub interface: String,
    pub scope: Option<RelationScope>,
    pub schema: Option<String>,
    #[serde(default)]
    pub versions: Vec<String>,
}

/// A charm's metadata.yaml file
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Metadata {
    /// Machine-friendly name of the charm
    pub name: String,

    /// Long-form description of the charm
    pub description: String,

    /// Tweetable summary of the charm
    pub summary: String,

    /// Containers for the charm
    #[serde(default)]
    pub containers: HashMap<String, Container>,

    /// Resources for the charm
    #[serde(default)]
    pub resources: HashMap<String, Resource>,

    /// Which other charms this charm requires a relation to in order to run
    #[serde(default)]
    pub requires: HashMap<String, Interface>,

    /// Which types of relations this charm provides
    #[serde(default)]
    pub provides: HashMap<String, Interface>,
}

/// A charm, as represented by the source directory
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharmSource {
    /// The path to the charm's source code
    source: PathBuf,

    /// The charm's config.yaml file
    pub config: Option<Config>,

    /// The charm's metadata.yaml file
    pub metadata: Metadata,
}

impl CharmSource {
    fn load_dir(source: &Path) -> Result<Self, JujuError> {
        let config: Option<Config> = read(source.join("config.yaml"))
            .map(|bytes| from_slice(&bytes))
            .unwrap_or(Ok(None))?;
        let metadata = from_slice(&read(source.join("metadata.yaml"))?)?;

        Ok(Self {
            source: source.into(),
            config,
            metadata,
        })
    }

    fn load_zip(source: &Path) -> Result<Self, JujuError> {
        let mut archive = ZipArchive::new(File::open(source)?)?;
        let config: Option<Config> = archive
            .by_name("config.yaml")
            .map(|mut zf| -> Result<_, JujuError> {
                let mut buf = String::new();
                zf.read_to_string(&mut buf)?;
                Ok(from_slice(buf.as_bytes())?)
            })
            .unwrap_or(Ok(None))?;

        let metadata = {
            let mut zf = archive.by_name("metadata.yaml")?;
            let mut buf = String::new();
            zf.read_to_string(&mut buf)?;
            from_slice(buf.as_bytes())?
        };

        Ok(Self {
            source: source.into(),
            config,
            metadata,
        })
    }

    /// Load a charm from its source directory
    pub fn load(source: &Path) -> Result<Self, JujuError> {
        if source.is_file() {
            Self::load_zip(source)
        } else {
            Self::load_dir(source)
        }
    }

    /// Build the charm from its source directory
    pub fn build(&self, destructive_mode: bool) -> Result<(), JujuError> {
        let source = self.source.to_string_lossy();
        let mut args = vec!["pack", "-p", &source];

        if destructive_mode {
            args.push("--destructive-mode")
        }

        cmd::run("charmcraft", &args)
    }

    pub fn artifact_path(&self) -> CharmURL {
        let mut path = current_dir().unwrap();
        path.push(&format!("{}_ubuntu-20.04-amd64.charm", self.metadata.name));
        CharmURL::from_path(path)
    }
    /// Push the charm to the charm store, and return the revision URL
    fn push(&self, cs_url: &str, resources: &HashMap<String, String>) -> Result<String, JujuError> {
        let dir = TempDir::new()?;

        let build_dir = {
            let zipped = self.artifact_path().to_string();
            let build_dir = dir.path().to_string_lossy();
            cmd::run("unzip", &[zipped.as_str(), "-d", &*build_dir])?;
            build_dir.to_string()
        };

        let resources = self.resources_with_defaults(resources)?;

        let args = vec!["push", &build_dir, cs_url]
            .into_iter()
            .map(String::from)
            .chain(
                resources
                    .iter()
                    .flat_map(|(k, v)| vec![String::from("--resource"), format!("{}={}", k, v)]),
            )
            .collect::<Vec<_>>();

        // Ensure all oci-image resources are pulled locally into Docker,
        // so that we can push them into the charm store
        for (name, value) in resources {
            let res = self.metadata.resources.get(&name).expect("Must exist!");

            if res.kind != ResourceType::OciImage {
                continue;
            }

            cmd::run("docker", &["pull", &value])?;
        }

        let mut output = cmd::get_output("charm", &args)?;

        // The command output is valid YAML that includes the URL that we care about, but
        // also includes output from `docker push`, so just chop out the first line that's
        // valid YAML.
        output.truncate(output.iter().position(|&x| x == 0x0a).unwrap());
        let push_metadata: HashMap<String, String> = from_slice(&output)?;
        let rev_url = push_metadata["url"].clone();

        // Attempt to tag the revision with the git commit, but ignore any failures
        // getting the commit.
        match cmd::get_output("git", &["rev-parse", "HEAD"]) {
            Ok(rev_output) => {
                let revision = String::from_utf8_lossy(&rev_output);
                cmd::run("charm", &["set", &rev_url, &format!("commit={}", revision)])?;
            }
            Err(err) => {
                println!(
                    "Error while getting git revision for {}, not tagging: `{}`",
                    self.metadata.name, err
                );
            }
        }

        Ok(rev_url)
    }

    /// Promote a charm from unpublished to the given channel
    fn promote(&self, rev_url: &str, to: &str) -> Result<(), JujuError> {
        let resources: Vec<HashMap<String, String>> = from_slice(&cmd::get_output(
            "charm",
            &["list-resources", rev_url, "--format", "yaml"],
        )?)?;

        let release_args = vec!["release", rev_url, "--channel", to]
            .into_iter()
            .map(String::from)
            .chain(resources.iter().flat_map(|r| {
                vec![
                    "--resource".to_string(),
                    format!("{}-{}", r["name"], r["revision"]),
                ]
            }))
            .collect::<Vec<_>>();

        cmd::run("charm", &release_args)
    }

    pub fn upload_charm_store(
        &self,
        url: &str,
        resources: &HashMap<String, String>,
        to: &[String],
        destructive_mode: bool,
    ) -> Result<String, JujuError> {
        self.build(destructive_mode)?;
        let rev_url = self.push(url, resources)?;

        for channel in to {
            self.promote(&rev_url, channel)?;
        }

        Ok(rev_url)
    }

    pub fn upload_charmhub(
        &self,
        url: &str,
        resources: &HashMap<String, String>,
        to: &[String],
        destructive_mode: bool,
    ) -> Result<String, JujuError> {
        self.build(destructive_mode)?;

        let resources = self.resources_with_defaults(resources)?;

        let resources: Vec<_> = resources
            .iter()
            .filter_map(|(name, value)| {
                let res = self.metadata.resources.get(name).expect("Must exist!");

                if res.kind != ResourceType::OciImage {
                    return None;
                }

                cmd::run(
                    "charmcraft",
                    &[
                        "upload-resource",
                        &self.metadata.name,
                        name,
                        "--image",
                        value,
                    ],
                )
                .unwrap();

                let output = cmd::get_stderr(
                    "charmcraft",
                    &["resource-revisions", &self.metadata.name, name],
                )
                .unwrap();
                let output = String::from_utf8_lossy(&output);
                let revision = output.lines().nth(1).unwrap().split(' ').next().unwrap();

                Some(format!("--resource={}:{}", name, revision))
            })
            .collect();

        let args: Vec<_> = vec!["upload".into(), self.artifact_path().to_string()]
            .into_iter()
            .chain(to.iter().map(|ch| format!("--release={}", ch)))
            .chain(resources)
            .collect();

        let mut output = cmd::get_stderr("charmcraft", &args)?;
        output.drain(0..9);
        output.truncate(output.iter().position(|&x| x == 0x20).unwrap());
        let revision = from_utf8(&output).unwrap().parse::<u32>().unwrap();

        Ok(CharmURL::parse(url)
            .unwrap()
            .with_revision(Some(revision))
            .to_string())
    }

    /// Merge default resources with resources given in e.g. a bundle.yaml
    pub fn resources_with_defaults(
        &self,
        configured: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, JujuError> {
        self.metadata
            .resources
            .iter()
            .map(|(k, v)| -> Result<(String, String), JujuError> {
                match (configured.get(k), &v.upstream_source) {
                    (Some(val), _) | (_, Some(val)) => Ok((k.clone(), val.clone())),
                    (None, None) => Err(JujuError::ResourceNotFound(
                        k.clone(),
                        self.metadata.name.clone(),
                    )),
                }
            })
            .collect()
    }
}
