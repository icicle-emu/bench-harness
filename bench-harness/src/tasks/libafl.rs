use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_interface::{client::Agent, RunCommand};
use anyhow::Context;

use crate::{
    tasks::{command_with_vars, try_copy, MonitorPidTask, WaitPidTask, SIGINT, SIGKILL},
    utils::Variables,
};

#[derive(serde::Deserialize, Clone, Debug)]
pub struct IcicleTask {
    pub duration_sec: f64,
    pub fuzzer_command: String,
    pub monitor_command: String,
    pub stats_src: String,
    pub stats_header: String,
    pub stats_dst: String,
    pub metadata_dir: Option<String>,
}

impl IcicleTask {
    pub fn run(&self, agent: &mut dyn Agent, vars: &Variables) -> anyhow::Result<()> {
        let workdir: PathBuf =
            vars.get("WORKDIR").ok_or_else(|| anyhow::format_err!("`WORKDIR` not defined"))?.into();

        tracing::debug!("creating working directory: `{}`", workdir.display());
        agent
            .run_task(mkdir_p_command(workdir.clone()))
            .with_context(|| format!("failed to create WORKDIR: {}", workdir.display()))?;

        let fuzzer_pid = agent.spawn_task(
            command_with_vars(&self.fuzzer_command, vars)?
                .stdin(agent_interface::Stdio::Null)
                .stdout(agent_interface::Stdio::File(workdir.join("fuzzer.stdout")))
                .stderr(agent_interface::Stdio::File(workdir.join("fuzzer.stderr"))),
        )?;
        tracing::debug!("fuzzer started with pid={fuzzer_pid}");

        let monitor_pid = agent.spawn_task(
            command_with_vars(&self.monitor_command, vars)?
                .stdin(agent_interface::Stdio::Null)
                .stdout(agent_interface::Stdio::File(workdir.join("monitor.stdout")))
                .stderr(agent_interface::Stdio::File(workdir.join("monitor.stderr"))),
        )?;
        tracing::debug!("monitor started with pid={monitor_pid}");

        MonitorPidTask::new(
            vec![fuzzer_pid, monitor_pid],
            Duration::from_secs_f64(self.duration_sec),
        )
        .run(agent)?;

        tracing::debug!("stopping monitor task (pid={fuzzer_pid})");
        if let Err(e) = agent.kill_process(monitor_pid, SIGINT) {
            tracing::warn!("Error sending SIGINT: {e:#}");
            agent.kill_process(monitor_pid, SIGKILL)?;
        }

        tracing::debug!("stopping fuzzer task (pid={fuzzer_pid})");
        if let Err(e) = agent.kill_process(fuzzer_pid, SIGINT) {
            tracing::warn!("Error sending SIGINT: {e:#}");
            agent.kill_process(fuzzer_pid, SIGKILL)?;
        }

        tracing::debug!("copying stats");
        let src_path = vars.expand_vars(&self.stats_src);
        let dst_path = vars.expand_vars(&self.stats_dst);
        copy_icicle_stats(agent, src_path.as_ref(), dst_path.as_ref(), &self.stats_header)?;

        if let Some(dir) = self.metadata_dir.as_ref() {
            let dir: PathBuf = vars.expand_vars(dir).into();
            let monitor_dir: Option<PathBuf> = vars.get("MONITOR_DIR").map(|x| x.into());

            copy_fuzzing_metadata(agent, &workdir, monitor_dir.as_deref(), &dir)?;
            tracing::debug!("copying fuzzing metadata to {}", dir.display());
        }

        Ok(())
    }
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct IcicleMonitorOnlyTask {
    pub monitor_command: String,
    pub stats_src: String,
    pub stats_header: String,
    pub stats_dst: String,
    pub copy_stdio_dir: Option<String>,
}

impl IcicleMonitorOnlyTask {
    pub fn run(&self, agent: &mut dyn Agent, vars: &Variables) -> anyhow::Result<()> {
        let workdir: PathBuf =
            vars.get("WORKDIR").ok_or_else(|| anyhow::format_err!("`WORKDIR` not defined"))?.into();

        tracing::debug!("creating working directory: `{}`", workdir.display());
        agent
            .run_task(mkdir_p_command(workdir.clone()))
            .with_context(|| format!("failed to create WORKDIR: {}", workdir.display()))?;

        let monitor_pid = agent.spawn_task(
            command_with_vars(&self.monitor_command, vars)?
                .stdin(agent_interface::Stdio::Null)
                .stdout(agent_interface::Stdio::File(workdir.join("monitor.stdout")))
                .stderr(agent_interface::Stdio::File(workdir.join("monitor.stderr"))),
        )?;
        tracing::debug!("monitor started with pid={monitor_pid}");

        WaitPidTask::new(vec![monitor_pid]).run(agent)?;

        tracing::debug!("copying stats");
        let src_path = vars.expand_vars(&self.stats_src);
        let dst_path = vars.expand_vars(&self.stats_dst);
        copy_icicle_stats(agent, src_path.as_ref(), dst_path.as_ref(), &self.stats_header)?;

        if let Some(dir) = self.copy_stdio_dir.as_ref() {
            let dir: PathBuf = vars.expand_vars(dir).into();
            tracing::debug!("copying stdout/stderr to {}", dir.display());
            std::fs::create_dir_all(&dir).with_context(|| {
                format!("failed to create {} to copy stdout/stderr", dir.display())
            })?;
            std::fs::write(
                dir.join("monitor.stdout"),
                &agent.read_file(workdir.join("monitor.stdout"))?,
            )?;
            std::fs::write(
                dir.join("monitor.stderr"),
                &agent.read_file(workdir.join("monitor.stderr"))?,
            )?;
        }
        Ok(())
    }
}

fn mkdir_p_command(dir: impl Into<std::ffi::OsString>) -> RunCommand {
    RunCommand::new("mkdir".into()).args(vec!["-p".into(), dir.into()])
}

fn copy_icicle_stats(
    agent: &mut dyn Agent,
    src_path: &Path,
    dst_path: &Path,
    header: &str,
) -> anyhow::Result<()> {
    let data = agent.read_file(src_path.into())?;

    let fs_guard = crate::HOST_FS_LOCK.lock();

    if let Some(parent) = dst_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut output_file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&dst_path)
        .with_context(|| format!("failed to open stats file: {}", dst_path.display()))?;

    // If the header has not been written by another trial then write it now.
    if output_file.metadata()?.len() == 0 {
        writeln!(output_file, "{}", header)?;
    }
    output_file.write_all(&data)?;
    output_file.flush()?;
    drop(fs_guard);

    Ok(())
}

fn copy_fuzzing_metadata(
    agent: &mut dyn Agent,
    workdir: &Path,
    monitor_dir: Option<&Path>,
    dir: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    try_copy(agent, workdir.join("monitor.stdout"), dir.join("monitor.stdout"), false);
    try_copy(agent, workdir.join("monitor.stderr"), dir.join("monitor.stderr"), false);

    try_copy(agent, workdir.join("fuzzer.stdout"), dir.join("fuzzer.stdout"), false);
    try_copy(agent, workdir.join("fuzzer.stderr"), dir.join("fuzzer.stderr"), false);

    if let Some(monitor_dir) = monitor_dir {
        try_copy(agent, monitor_dir.join("hit_blocks.json"), dir.join("hit_blocks.json"), false);
        try_copy(agent, monitor_dir.join("input_graph.json"), dir.join("input_graph.json"), false);
    }

    Ok(())
}
