use std::path::{Path, PathBuf};

use agent_interface::{client::Agent, DirEntry};
use anyhow::Context;

/// AFL plot data in AFL++ v4 format.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PlotDataRowV4 {
    pub relative_time: u64,
    pub cycles_done: u64,
    pub cur_item: u64,
    pub corpus_count: u64,
    pub pending_total: u64,
    pub pending_favs: u64,
    #[serde(deserialize_with = "deserialize_percent")]
    pub map_size: f64,
    pub saved_crashes: u64,
    pub saved_hangs: u64,
    pub max_depth: u64,
    pub execs_per_sec: f64,
    pub total_execs: u64,
    pub edges_found: u64,
}

impl PlotDataRowV4 {
    pub const FIELDS: &'static [&'static str] = &[
        "relative_time",
        "cycles_done",
        "cur_item",
        "corpus_count",
        "pending_total",
        "pending_favs",
        "map_size",
        "saved_crashes",
        "saved_hangs",
        "max_depth",
        "execs_per_sec",
        "total_execs",
        "edges_found",
    ];

    pub fn from_reader<R>(reader: R) -> anyhow::Result<Vec<Self>>
    where
        R: std::io::Read,
    {
        parse_plot_data(reader)
    }
}

fn deserialize_percent<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    use std::borrow::{Borrow, Cow};

    let text: Cow<str> = serde::Deserialize::deserialize(deserializer)?;
    let text_str: &str = text.borrow();

    Ok(text_str
        .trim_end_matches("%")
        .parse::<f64>()
        .map_err(|err| serde::de::Error::custom(err.to_string()))?
        * 100.0)
}

pub fn parse_plot_data<R, T>(reader: R) -> anyhow::Result<Vec<T>>
where
    R: std::io::Read,
    T: serde::de::DeserializeOwned,
{
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .comment(Some(b'#'))
        .has_headers(false)
        .from_reader(reader);

    let mut out = vec![];

    let mut total_errors = 0;
    for result in reader.deserialize() {
        let data: T = match result {
            Ok(data) => data,
            Err(e) => {
                total_errors += 1;
                tracing::warn!("parse error: {:#}", e);
                if total_errors > 10 {
                    anyhow::bail!(">10 parse errors: {:#}", e);
                }
                continue;
            }
        };
        out.push(data);
    }

    Ok(out)
}

/// Get the list of afl inputs inside of `path`
pub fn input_entries(
    agent: &mut dyn Agent,
    path: PathBuf,
) -> anyhow::Result<Vec<agent_interface::DirEntry>> {
    let mut out = vec![];
    for entry in agent.read_dir(path).context("failed to read dir")? {
        if !entry.is_file || entry.path.ends_with("README.txt") {
            continue;
        }
        out.push(entry);
    }
    Ok(out)
}

/// Attempt to get the time since the start of the fuzzing session of a file using the data tagged
/// by AFL++ if possible.
pub fn get_relative_time(file: &DirEntry, start_time: std::time::SystemTime) -> u64 {
    // AFL++ stores the relative time in the file name, attempt to extract it here
    if let Some(name) = file.path.file_name().and_then(|x| x.to_str()) {
        if let Some((time, _)) = name
            .split_once("time")
            .and_then(|(_, rest)| rest.strip_prefix(|x: char| !x.is_numeric()))
            .and_then(|x| x.split_once(","))
        {
            if let Ok(time) = time.parse() {
                return time;
            }
        }
    }

    // Failed to parse time from file name so attempt to infer the relative time from the file
    // creation time relative to the parent directory.
    file.modified.duration_since(start_time).map_or(0, |t| t.as_millis() as u64)
}

/// Get the input id encoded in the filename by AFL++.
pub fn get_input_id(file: &Path) -> Option<u64> {
    let name = file.file_name().and_then(|x| x.to_str())?;
    let (id, _) = name
        .split_once("id")
        .and_then(|(_, rest)| rest.strip_prefix(|x: char| !x.is_numeric()))
        .and_then(|x| x.split_once(","))?;
    id.parse().ok()
}
