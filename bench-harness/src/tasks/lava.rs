//! Module for verifying LAVA-M bugs

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::PathBuf,
};

use agent_interface::{client::Agent, ExitKind, RunCommand};
use anyhow::Context;

use crate::{
    afl,
    tasks::{append_csv, command_with_vars},
    utils::Variables,
};

#[derive(serde::Deserialize, Clone, Debug)]
pub struct LavaVerifierTask {
    command: String,
    crash_dir: String,
    dst: String,
}

impl LavaVerifierTask {
    pub fn run(&self, agent: &mut dyn Agent, vars: &Variables) -> anyhow::Result<()> {
        let tag = vars.get("TAG").unwrap_or("?");

        let crash_dir = vars.expand_vars(&self.crash_dir);
        let start_time = agent
            .stat(PathBuf::from(crash_dir.clone()))
            .with_context(|| format!("Failed to stat: {crash_dir}"))?
            .modified;
        let crashes = afl::input_entries(agent, crash_dir.into())?;

        let command = command_with_vars(&self.command, vars)?;
        let results = check(agent, command, start_time, crashes)?;
        let bugs =
            results.into_iter().map(|(bug_id, status)| (tag, bug_id.to_string(), status.time));

        let dst: PathBuf = vars.expand_vars(&self.dst).into();
        append_csv(
            dst,
            b"tag,bug_id,time",
            [(tag, "none".to_string(), 0)].into_iter().chain(bugs),
        )?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct CheckStatus {
    /// The name the file that was checked.
    #[allow(unused)]
    name: String,

    /// The time that the file was generated at.
    time: u64,

    /// How the input exited when it was run by the host.
    #[allow(unused)]
    exit: ExitKind,
}

#[derive(serde::Serialize)]
struct Row {
    bug_id: u64,
    time: u64,
}

fn check(
    agent: &mut dyn Agent,
    command: RunCommand,
    start_time: std::time::SystemTime,
    mut crashes: Vec<agent_interface::DirEntry>,
) -> anyhow::Result<BTreeMap<u64, CheckStatus>> {
    const MAX_ATTEMPTS: usize = 10;

    let mut bugs: BTreeMap<u64, CheckStatus> = BTreeMap::new();

    let mut lava_verifier = LavaVerifier::new(command)?;
    for i in 0..MAX_ATTEMPTS {
        if crashes.is_empty() {
            break;
        }

        tracing::debug!("Verifying {} crashes, round {}/{}", crashes.len(), i + 1, MAX_ATTEMPTS);
        for entry in std::mem::take(&mut crashes) {
            anyhow::ensure!(!crate::should_stop(), "stop requested");

            match lava_verifier.check_one(agent, &entry) {
                Ok((exit, found_bugs)) if found_bugs.is_empty() => {
                    if matches!(exit, ExitKind::Crash) && i + 1 < MAX_ATTEMPTS {
                        // Sometimes crashes are non-deterministic on the host so try again.
                        crashes.push(entry);
                    }
                    else {
                        tracing::warn!("`{}`: no bug id: {:?}", entry.path.display(), exit);
                    }
                }
                Ok((exit, found_bugs)) => {
                    let input_id = afl::get_input_id(&entry.path).unwrap_or(100000000000);
                    tracing::trace!("{}: {:?}", input_id, found_bugs);

                    let name: String =
                        entry.path.file_name().and_then(OsStr::to_str).unwrap_or("unknown").into();
                    let time = afl::get_relative_time(&entry, start_time);

                    for id in found_bugs {
                        let status = CheckStatus { name: name.clone(), time, exit };
                        match bugs.entry(id) {
                            std::collections::btree_map::Entry::Vacant(slot) => {
                                slot.insert(status);
                            }
                            std::collections::btree_map::Entry::Occupied(mut entry) => {
                                if entry.get_mut().time > time {
                                    *entry.get_mut() = status;
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("Error verifying {}: {:#}", entry.path.display(), e),
            }
        }
    }

    Ok(bugs)
}

struct LavaVerifier {
    command: RunCommand,
    input_index: Option<usize>,
    matcher: regex::bytes::Regex,
}

impl LavaVerifier {
    fn new(command: RunCommand) -> anyhow::Result<Self> {
        // Find where in the arguments the input string needs to be substituted into.
        let input_index = command.args.iter().position(|x| x == "@@");

        // Construct a regex for matching bug IDs.
        //
        // Note: it is important that we match the entire regex since thanks to comparison logging
        // instrumentation we may end up with fragments of the the bug strings copied into the
        // input.
        let matcher = regex::bytes::Regex::new(r"Successfully triggered bug ([0-9]+)").unwrap();

        Ok(Self { command, input_index, matcher })
    }

    fn check_one(
        &mut self,
        agent: &mut dyn Agent,
        input: &agent_interface::DirEntry,
    ) -> anyhow::Result<(ExitKind, BTreeSet<u64>)> {
        if let Some(index) = self.input_index {
            self.command.args[index] = input.path.clone().into();
        }
        self.command.stdin = agent_interface::Stdio::File(input.path.to_owned());

        let output = agent.run_task(self.command.clone())?;

        let find_lava_bugs = |out: &[u8], bugs: &mut BTreeSet<u64>| {
            tracing::trace!("lava output: {:?}", out.escape_ascii());
            for entry in self.matcher.captures_iter(out) {
                let bug_id_bytes = entry.get(1).unwrap().as_bytes();
                bugs.insert(std::str::from_utf8(bug_id_bytes).unwrap().parse::<u64>().unwrap());
            }
        };

        let mut bugs = BTreeSet::new();
        find_lava_bugs(&output.stdout, &mut bugs);
        find_lava_bugs(&output.stderr, &mut bugs);

        Ok((output.exit, bugs))
    }
}
