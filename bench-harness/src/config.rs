use std::{collections::HashMap, path::PathBuf};

use anyhow::Context;
use indexmap::IndexMap;

use crate::{
    firecracker, image_builder::ImageSource, tasks::DynamicTask, utils::Variables,
    worker::LocalWorker,
};

fn default_cache_dir() -> PathBuf {
    ".harness-cache".into()
}

#[derive(serde::Deserialize)]
pub(crate) struct CacheConfig {
    #[serde(default = "default_cache_dir")]
    pub dir: PathBuf,

    /// Controls whether the image builder should skip validation.
    #[serde(default)]
    pub skip_validation: bool,

    /// Avoid using cached disk images.
    #[serde(default)]
    pub disable_image_cache: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct ConfigData {
    #[serde(default)]
    pub images: IndexMap<String, ImageSource>,
    #[serde(default)]
    pub instances: IndexMap<String, Instance>,
    #[serde(default)]
    pub templates: HashMap<String, TaskTemplate>,
    #[serde(default)]
    pub templated_tasks: HashMap<String, TemplatedTask>,
    #[serde(default)]
    pub tasks: HashMap<String, TaskConfig>,
}

impl ConfigData {
    pub fn merge(&mut self, other: ConfigData) -> anyhow::Result<()> {
        macro_rules! checked_insert {
            ($src:expr, $dst:expr, $name:expr) => {{
                for (key, value) in $src {
                    if $dst.contains_key(&key) {
                        anyhow::bail!("redefinition of {} {}", $name, key);
                    }
                    $dst.insert(key, value);
                }
            }};
        }
        checked_insert!(other.images, self.images, "image");
        checked_insert!(other.instances, self.instances, "instance");
        checked_insert!(other.templates, self.templates, "template");
        checked_insert!(other.templated_tasks, self.templated_tasks, "templated_task");
        checked_insert!(other.tasks, self.tasks, "task");

        Ok(())
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub vars: Vec<KeyValue>,
    pub local_worker: Option<LocalWorker>,
    #[serde(flatten)]
    pub cache: CacheConfig,
    pub firecracker: Option<FirecrackerBin>,
    pub kernel: Option<Kernel>,

    #[serde(default)]
    pub include: Vec<PathBuf>,

    #[serde(default)]
    pub include_template: Vec<PathBuf>,

    #[serde(flatten)]
    pub data: ConfigData,
}

impl Config {
    pub(crate) fn get_task(&self, name: &str) -> anyhow::Result<TaskConfig> {
        if let Some(task) = self.data.tasks.get(name) {
            return Ok(task.clone());
        }

        if let Some(templated) = self.data.templated_tasks.get(name) {
            let template_name = &templated.template;
            let template = self
                .data
                .templates
                .get(template_name)
                .ok_or_else(|| anyhow::format_err!("undefined template `{template_name}`"))?;

            let mut vars = Variables::default();
            vars.insert_all(template.defaults.iter().cloned().map(|x| x.into()));
            vars.insert_all(templated.parameters.iter().cloned().map(|x| x.into()));

            let mut vars: HashMap<&str, &str> = HashMap::new();
            vars.extend(template.defaults.iter().map(|x| (x.key.as_str(), x.value.as_str())));
            vars.extend(templated.parameters.iter().map(|x| (x.key.as_str(), x.value.as_str())));

            let mut env = minijinja::Environment::new();
            env.add_template(template_name, &template.template)?;
            env.add_filter("remove_newlines", remove_newlines);

            let template = env.get_template(template_name)?;
            let expanded_template = template.render(vars)?;

            let task = toml::from_str(&expanded_template).map_err(|e| {
                anyhow::format_err!("invalid template expansion {e}: {expanded_template}")
            })?;

            return Ok(task);
        }

        anyhow::bail!("task {name} not found");
    }
}

pub(crate) fn toml_from_path<T>(path: &std::path::Path) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read: {}", path.display()))?;
    Ok(toml::from_slice(&bytes).with_context(|| format!("failed to parse: {}", path.display()))?)
}

pub(crate) fn config_data_from_template<T>(path: &std::path::Path) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let name = format!("{}", path.display());
    let template =
        std::fs::read_to_string(path).with_context(|| format!("failed to read: {name}"))?;

    let mut env = minijinja::Environment::new();
    env.add_template(&name, &template)?;

    let template = env.get_template(&name)?;
    let expanded_template = template.render(&HashMap::<&str, &str>::new())?;

    match path.file_stem().and_then(|x| x.to_str()).and_then(|x| x.split_once('.')) {
        Some((_, "toml")) => toml::from_str(&expanded_template)
            .map_err(|e| anyhow::format_err!("invalid toml file: {e}: {expanded_template}")),
        Some((_, "ron")) => ron::from_str(&expanded_template)
            .map_err(|e| anyhow::format_err!("invalid ron file: {e}: {expanded_template}")),
        _ => anyhow::bail!("Unknown template format for {name}"),
    }
}

fn remove_newlines(_state: &minijinja::State, value: String) -> Result<String, minijinja::Error> {
    Ok(value.replace("\n", "").replace("\r", ""))
}

#[derive(serde::Deserialize)]
pub(crate) struct FirecrackerBin {
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(serde::Deserialize)]
pub(crate) struct Kernel {
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub path: Option<PathBuf>,
    pub boot_args: String,
    pub entropy: Option<Vec<u32>>,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// The drive will be mounted as read only.
    ReadOnly,

    /// The drive will be copied to the tmp directory and mounted as read/write.
    Duplicate,

    /// The drive will be mounted as read/write in-place.
    InPlace,

    /// Re-use a duplicated image from a prior run.
    ReuseDuplicate,
}

#[derive(serde::Deserialize)]
pub(crate) struct DriveConfig {
    pub name: String,
    pub image: String,
    pub mount_as: MountKind,
}

fn default_true() -> bool {
    true
}

fn default_5s() -> u64 {
    5
}

#[derive(serde::Deserialize)]
pub(crate) struct Instance {
    #[serde(default = "default_5s")]
    pub boot_delay_sec: u64,
    pub machine: firecracker::MachineConfig,
    pub rootfs: DriveConfig,
    pub drives: Vec<DriveConfig>,
    #[serde(default = "default_true")]
    pub recreate_workdir: bool,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct TaskTemplate {
    pub defaults: Vec<KeyValue>,
    pub template: String,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct TemplatedTask {
    pub template: String,
    pub parameters: Vec<KeyValue>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub(crate) struct TaskConfig {
    pub instance: String,
    pub vars: Vec<KeyValue>,
    pub tasks: Vec<DynamicTask>,
}

#[derive(Debug, Clone)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }

    /// Parses [KeyValue] from a string (e.g. "KEY=VALUE")
    pub fn from_str(input: &str) -> Option<Self> {
        let pos = input.find("=")?;
        let (key, value) = input.split_at(pos);
        Some(Self { key: key.trim().to_owned(), value: value[1..].trim().to_owned() })
    }
}

impl Into<(String, String)> for KeyValue {
    fn into(self) -> (String, String) {
        (self.key, self.value)
    }
}

impl<'de> serde::Deserialize<'de> for KeyValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use std::borrow::{Borrow, Cow};

        let text: Cow<str> = serde::Deserialize::deserialize(deserializer)?;
        Self::from_str(text.borrow()).ok_or_else(|| serde::de::Error::custom("expected KEY=VALUE"))
    }
}

impl serde::Serialize for KeyValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("{}={}", self.key, self.value))
    }
}
