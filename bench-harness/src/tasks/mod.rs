mod lava;
mod libafl;

use std::{collections::HashMap, io::Write, path::PathBuf, time::Duration};

use agent_interface::{client::Agent, ExitKind, RunCommand};
use anyhow::Context;

use crate::{config::KeyValue, utils::Variables};

pub trait Runable: Send {
    fn run(&mut self, vars: Variables, agent: &mut dyn Agent) -> anyhow::Result<()>;
}

pub struct Task {
    pub name: String,
    pub instance: String,
    pub vars: Vec<KeyValue>,
    pub runable: Box<dyn Runable>,
}

impl Task {
    pub fn run(&mut self, worker_id: usize, agent: &mut dyn Agent) -> anyhow::Result<()> {
        let mut globals = Variables::default();
        globals.insert("WORKER_ID".into(), worker_id.to_string());
        globals.insert_all(self.vars.iter().map(|x| x.clone().into()));

        self.runable.run(globals, agent)
    }
}

fn default_true() -> bool {
    true
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DynamicTask {
    Run {
        command: String,
        stdout: Option<String>,
        stderr: Option<String>,
        #[serde(default, deserialize_with = "crate::utils::parse_duration")]
        duration: Option<Duration>,
    },
    SpawnTask {
        key: String,
        command: String,
        stdout: Option<String>,
        stderr: Option<String>,
    },
    ResultCollector {
        command: String,
        dst: String,
    },
    Sleep {
        time_sec: f64,
    },
    Kill {
        signal: i32,
        tasks: Vec<String>,
    },
    CopyFile {
        src: String,
        dst: String,
        #[serde(default = "default_true")]
        append: bool,
    },
    CopyDir {
        src: String,
        dst: String,
    },
    /// Merges the data from `src` to the file at `dst` after adding a prefix to each line.
    MergeWithPrefix {
        prefix: String,
        header: String,
        src: String,
        dst: String,
    },
    RunHost {
        command: String,
        stdout: Option<String>,
        stderr: Option<String>,
    },
    LibaflIcicleBenchmark(libafl::IcicleTask),
    LibaflIcicleMonitorOnly(libafl::IcicleMonitorOnlyTask),
    LavaVerifier(lava::LavaVerifierTask),
    InputPatternVerifier(InputPatternVerifier),
    SaveTaggedAflPlotDataV4(SaveTaggedAflPlotDataV4),
    TaskList {
        tasks: Vec<DynamicTask>,
    },
}

impl Runable for DynamicTask {
    fn run(&mut self, vars: Variables, agent: &mut dyn Agent) -> anyhow::Result<()> {
        if crate::should_stop() {
            anyhow::bail!("exited without finishing task");
        }
        let mut pids = HashMap::new();
        if !matches!(self, DynamicTask::TaskList { .. }) {
            tracing::info!("Running: {self:?}");
        }
        match self {
            DynamicTask::Run { command, stdout, stderr, duration } => match duration {
                Some(t) => run_timed_task(agent, command, &vars, stdout, stderr, *t)?,
                None => run_task(agent, command, &vars, stdout, stderr)?,
            },
            DynamicTask::SpawnTask { key, command, stdout, stderr } => {
                let pid = agent.spawn_task(
                    command_with_vars(&command, &vars)?
                        .stdin(agent_interface::Stdio::Null)
                        .stdout(get_stdio(stdout, &vars))
                        .stderr(get_stdio(stderr, &vars)),
                )?;
                pids.insert(key, pid);
            }
            DynamicTask::ResultCollector { command, dst } => {
                let dst = vars.expand_vars(&dst);
                let result = agent.run_task(command_with_vars(&command, &vars)?)?;
                match result.exit {
                    ExitKind::Success => std::fs::write(dst, result.stdout)?,
                    ExitKind::Exit(code) => {
                        anyhow::bail!(
                            "exit: {}, stdout: {}, stderr: {}",
                            code,
                            result.stdout.escape_ascii(),
                            result.stderr.escape_ascii()
                        )
                    }
                    ExitKind::Crash => {
                        anyhow::bail!(
                            "crash, stdout: {}, stderr: {}",
                            result.stdout.escape_ascii(),
                            result.stderr.escape_ascii()
                        )
                    }
                    ExitKind::Hang => anyhow::bail!("hang"),
                }
            }
            DynamicTask::RunHost { command, stdout, stderr } => {
                let output = command_with_vars(&command, &vars)?
                    .stdin(agent_interface::Stdio::Null)
                    .stdout(get_stdio(stdout, &vars))
                    .stderr(get_stdio(stderr, &vars))
                    .spawn()?
                    .wait()?;
                if !output.success() {
                    tracing::info!("Error: {output:?}");
                }
            }
            DynamicTask::Sleep { time_sec } => {
                let start_time = std::time::Instant::now();
                crossbeam_channel::select! {
                    recv(crate::cancellation_channel()) -> _ => {
                        anyhow::bail!("early exit: {:?} (task canceled)", start_time.elapsed());
                    }
                    default(Duration::from_secs_f64(*time_sec)) => {},
                };
            }
            DynamicTask::Kill { signal, tasks } => {
                for task in tasks {
                    let pid = pids
                        .get(&task)
                        .ok_or_else(|| anyhow::format_err!("task {} not found", task))?;
                    agent.kill_process(*pid, *signal)?;
                }
            }
            DynamicTask::CopyFile { src, dst, append } => {
                let src: PathBuf = vars.expand_vars(&src).into();
                let dst: PathBuf = vars.expand_vars(&dst).into();
                try_copy(agent, src, dst, *append);
            }
            DynamicTask::CopyDir { src, dst } => {
                let src: PathBuf = vars.expand_vars(&src).into();
                let dst: PathBuf = vars.expand_vars(&dst).into();
                try_copy_dir(agent, src, dst);
            }
            DynamicTask::MergeWithPrefix { prefix, header, src, dst } => {
                let prefix = vars.expand_vars(&prefix);
                let data = agent.read_file(vars.expand_vars(&src).into())?;
                let dst: PathBuf = vars.expand_vars(&dst).into();
                merge_with_prefix(dst, &header, &prefix, &data)?;
            }
            DynamicTask::LibaflIcicleBenchmark(inner) => inner.run(agent, &vars)?,
            DynamicTask::LibaflIcicleMonitorOnly(inner) => inner.run(agent, &vars)?,
            DynamicTask::LavaVerifier(inner) => inner.run(agent, &vars)?,
            DynamicTask::InputPatternVerifier(inner) => inner.run(agent, &vars)?,
            DynamicTask::SaveTaggedAflPlotDataV4(inner) => inner.run(agent, &vars)?,
            DynamicTask::TaskList { tasks: subtasks } => {
                for task in subtasks {
                    task.run(vars.clone(), agent)?;
                }
            }
        }

        Ok(())
    }
}

fn run_task(
    agent: &mut dyn Agent,
    command: &mut String,
    vars: &Variables,
    stdout: &Option<String>,
    stderr: &Option<String>,
) -> Result<(), anyhow::Error> {
    let pid = agent.spawn_task(
        command_with_vars(&command, vars)?
            .stdin(agent_interface::Stdio::Null)
            .stdout(get_stdio(stdout, vars))
            .stderr(get_stdio(stderr, vars)),
    )?;
    agent.wait_pid(pid)?;
    Ok(())
}

fn run_timed_task(
    agent: &mut dyn Agent,
    command: &mut String,
    vars: &Variables,
    stdout: &Option<String>,
    stderr: &Option<String>,
    duration: Duration,
) -> Result<(), anyhow::Error> {
    let pid = agent.spawn_task(
        command_with_vars(&command, vars)?
            .stdin(agent_interface::Stdio::Null)
            .stdout(get_stdio(stdout, vars))
            .stderr(get_stdio(stderr, vars)),
    )?;
    tracing::debug!("task started with pid={pid}");
    MonitorPidTask::new(vec![pid], duration).run(agent)?;

    tracing::debug!("stopping task (pid={pid})");
    if let Err(e) = agent.kill_process(pid, SIGINT) {
        tracing::warn!("Error sending SIGINT: {e:#}");
        agent.kill_process(pid, SIGKILL)?;
    }

    Ok(())
}

fn get_stdio(value: &Option<String>, vars: &Variables) -> agent_interface::Stdio {
    value
        .as_ref()
        .map(|x| agent_interface::Stdio::File(vars.expand_vars(&x).into()))
        .unwrap_or(agent_interface::Stdio::Inherit)
}

struct MonitorPidTask {
    pids: Vec<u32>,
    duration: Duration,
    tick: Duration,
}

impl MonitorPidTask {
    fn new(pids: Vec<u32>, duration: Duration) -> Self {
        Self { pids, duration, tick: Duration::from_secs(5) }
    }

    fn run(&self, agent: &mut dyn Agent) -> anyhow::Result<()> {
        let start_time = std::time::Instant::now();
        let cancel = crate::cancellation_channel();
        let deadline = crossbeam_channel::after(self.duration);
        loop {
            crossbeam_channel::select! {
                recv(deadline) -> _ => break,
                recv(cancel) -> _ => {
                    anyhow::bail!("early exit: {:?} (task canceled)", start_time.elapsed());
                }
                default(self.tick) => {
                    for pid in &self.pids {
                        if agent.get_status(*pid)?.is_none() {
                            if self.duration != Duration::MAX {
                                tracing::warn!(
                                    "early exit: {:?} (pid={pid} stopped)", start_time.elapsed()
                                );
                            }
                            return Ok(())
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

struct WaitPidTask {
    pids: Vec<u32>,
    tick: Duration,
}

impl WaitPidTask {
    fn new(pids: Vec<u32>) -> Self {
        Self { pids, tick: Duration::from_secs(2) }
    }

    fn run(&self, agent: &mut dyn Agent) -> anyhow::Result<()> {
        let cancel = crate::cancellation_channel();
        loop {
            crossbeam_channel::select! {
                recv(cancel) -> _ => anyhow::bail!("(task canceled)"),
                default(self.tick) => {
                    for pid in &self.pids {
                        if agent.get_status(*pid)?.is_none() {
                            return Ok(())
                        }
                    }
                }
            }
        }
    }
}

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;
// const SIGTERM: i32 = 15;

fn try_copy(agent: &mut dyn Agent, from: PathBuf, to: PathBuf, append: bool) {
    let data = match agent.read_file(from.clone()) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("reading {} from agent: {e:?}", from.display());
            return;
        }
    };

    let fs_guard = crate::HOST_FS_LOCK.lock();
    if let Some(parent) = to.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut file = match std::fs::OpenOptions::new().create(true).append(append).open(&to) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("error opening: {}: {e:?}", to.display());
            return;
        }
    };

    if let Err(e) = file.write_all(&data) {
        tracing::warn!("error writing data to {}: {e:?}", to.display());
    }

    let _ = file.flush();
    drop(fs_guard);
}

fn try_copy_dir(agent: &mut dyn Agent, from: PathBuf, to: PathBuf) {
    let dir = match agent.read_dir(from.clone()) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("reading {} from agent: {e:?}", from.display());
            return;
        }
    };

    let fs_guard = crate::HOST_FS_LOCK.lock();

    for entry in dir {
        if !entry.is_file {
            // @todo: handle recursive copying.
            continue;
        }

        let dst_path = match entry.path.file_name() {
            Some(name) => to.join(name),
            None => continue,
        };
        if let Some(parent) = dst_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let data = match agent.read_file(entry.path.clone()) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("reading {} from agent: {e:?}", entry.path.display());
                continue;
            }
        };

        if let Err(e) = std::fs::write(&dst_path, &data) {
            tracing::warn!("error writing data to {}: {e:?}", dst_path.display());
        }
    }
    drop(fs_guard);
}

fn command_with_vars(command: &str, vars: &Variables) -> anyhow::Result<RunCommand> {
    let cmd_string = vars.expand_vars(&command);
    let mut cmd = RunCommand::from_cmd_string(&cmd_string)
        .ok_or_else(|| anyhow::format_err!("failed to parse command: {}", cmd_string))?;
    cmd.vars.extend(vars.iter().map(|(k, v)| (k.into(), v.into())));
    Ok(cmd)
}

pub fn append_csv<T>(
    dst: PathBuf,
    header: &[u8],
    rows: impl Iterator<Item = T>,
) -> anyhow::Result<()>
where
    T: serde::Serialize,
{
    let fs_guard = crate::HOST_FS_LOCK.lock();

    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut output = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&dst)
        .with_context(|| format!("failed to open: {}", dst.display()))?;

    if output.metadata()?.len() == 0 {
        output.write_all(header)?;
        output.write_all(b"\n")?;
    }

    let mut writer = csv::WriterBuilder::new().has_headers(false).from_writer(output);
    for row in rows {
        writer.serialize(row)?;
    }

    drop(fs_guard);
    Ok(())
}

pub fn resolve_bug_ids<T, F>(
    agent: &mut dyn Agent,
    crash_dir: PathBuf,
    mut resolve_input: F,
) -> anyhow::Result<Vec<(String, u64)>>
where
    T: Eq + Ord + ToString,
    F: FnMut(&mut dyn Agent, &agent_interface::DirEntry) -> anyhow::Result<Vec<T>>,
{
    let start_time = agent
        .stat(PathBuf::from(crash_dir.clone()))
        .with_context(|| format!("Failed to stat: {}", crash_dir.display()))?
        .modified;

    let crashes = crate::afl::input_entries(agent, crash_dir.into())?;

    let mut bugs: std::collections::BTreeMap<T, u64> = std::collections::BTreeMap::new();
    for entry in crashes {
        anyhow::ensure!(!crate::should_stop(), "stop requested");

        let time = crate::afl::get_relative_time(&entry, start_time);
        for bug_id in resolve_input(agent, &entry)? {
            match bugs.entry(bug_id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(time);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    *entry.get_mut() = (*entry.get()).min(time);
                }
            }
        }
    }

    Ok(bugs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[derive(Clone, serde::Deserialize)]
struct Pattern {
    key: String,
    #[serde(default)]
    offset: usize,
    #[serde(deserialize_with = "byte_array_or_string")]
    bytes: Vec<u8>,
}

impl std::fmt::Debug for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pattern")
            .field("key", &self.key)
            .field("offset", &self.offset)
            .field("bytes", &format!("{}", self.bytes.escape_ascii()))
            .finish()
    }
}

fn byte_array_or_string<'de, D>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(untagged)]
    enum BytesOrString {
        Bytes(Vec<u8>),
        String(String),
    }
    let bytes_or_string: BytesOrString = serde::Deserialize::deserialize(deserializer)?;
    match bytes_or_string {
        BytesOrString::Bytes(bytes) => Ok(bytes),
        BytesOrString::String(string) => Ok(string.into_bytes()),
    }
}

/// A verifier that works by looking for a patterns in the input file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InputPatternVerifier {
    crash_dir: String,
    dst: String,
    patterns: Vec<Pattern>,
}

impl InputPatternVerifier {
    pub fn run(&self, agent: &mut dyn Agent, vars: &Variables) -> anyhow::Result<()> {
        let tag = vars.get("TAG").unwrap_or("?");

        let crash_dir = vars.expand_vars(&self.crash_dir);
        let bugs = resolve_bug_ids(agent, crash_dir.into(), |agent, entry| {
            let data = agent.read_file(entry.path.clone())?;

            for pattern in &self.patterns {
                let bytes = data.get(pattern.offset..).unwrap_or(&[]);
                if bytes.starts_with(&pattern.bytes) {
                    return Ok(vec![pattern.key.clone()]);
                }
            }

            tracing::warn!("`{}`: no bug id", entry.path.display());
            Ok(vec![])
        })?;

        let dst: PathBuf = vars.expand_vars(&self.dst).into();
        let rows = bugs.into_iter().map(|(bug_id, time)| (tag, bug_id, time));
        // Add a dummy bug to avoid droping trials when there are no bugs.
        append_csv(dst, b"tag,bug_id,time", [(tag, "none".into(), 0)].into_iter().chain(rows))?;

        Ok(())
    }
}

/// Saves plot data in AFL++ v4 format to a file after applying a tag.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SaveTaggedAflPlotDataV4 {
    workdir: String,
    dst: String,
}

impl SaveTaggedAflPlotDataV4 {
    pub fn run(&self, agent: &mut dyn Agent, vars: &Variables) -> anyhow::Result<()> {
        let tag = vars.get("TAG").unwrap_or("?");

        let plot_data = format!("{}/default/plot_data", vars.expand_vars(&self.workdir));
        let data = agent.read_file(plot_data.into())?;
        let rows = crate::afl::PlotDataRowV4::from_reader(std::io::Cursor::new(data))?;

        let header = format!("tag,{}", crate::afl::PlotDataRowV4::FIELDS.join(","));

        let dst: PathBuf = vars.expand_vars(&self.dst).into();
        append_csv(dst, header.as_bytes(), rows.into_iter().map(|row| (tag, row)))?;

        Ok(())
    }
}

pub fn merge_with_prefix(
    dst: PathBuf,
    header: &str,
    prefix: &str,
    data: &[u8],
) -> anyhow::Result<()> {
    let fs_guard = crate::HOST_FS_LOCK.lock();

    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut output = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&dst)
        .with_context(|| format!("failed to open: {}", dst.display()))?;

    if output.metadata()?.len() == 0 {
        output.write_all(header.as_bytes())?;
        output.write_all(b"\n")?;
    }

    output.write_all(prefix.as_bytes())?;
    output.write_all(b"\n")?;

    output.write_all(data)?;

    drop(fs_guard);
    Ok(())
}
