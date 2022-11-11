pub mod client;
pub mod command;
pub mod utils;

use std::{ffi::OsString, path::PathBuf};

use anyhow::Context;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_file: bool,
    pub len: u64,
    pub modified: std::time::SystemTime,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// Attempt to reboot the VM.
    Reboot,

    /// Exit the agent, waiting for it to be restarted by systemd.
    RestartAgent,

    /// Retrive all statsd stats that have occured since we last made this request.
    GetStats,

    /// Run a raw process in the background.
    SpawnProcess(RunCommand),

    /// Run a subprocess to completion, returning stdout and stderr.
    RunProcess(RunCommand),

    /// Waits for a subprocess to exit, returning the exit code.
    WaitPid(u32),

    /// Get the status of the process associated with the given PID.
    GetStatus(u32),

    /// Send a signal to a process managed by the VM.
    KillProcess { pid: u32, signal: i32 },

    /// Read a file from the file system.
    ReadFile { path: PathBuf, offset: u64, len: Option<u64> },

    /// Read metadata about a file.
    StatFile(PathBuf),

    /// Read the content of a directory from the file system.
    ReadDir(PathBuf),

    /// Add entropy to the system.
    AddEntropy(Vec<u32>),

    /// Perform multiple commands in a single request.
    Bulk(Vec<Request>),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Response {
    Error { error: String },
    Value(serde_json::Value),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stdio {
    Null,
    File(PathBuf),
    Inherit,
}

impl Stdio {
    pub fn to_stdio(&self, dir: Option<&std::path::Path>) -> anyhow::Result<std::process::Stdio> {
        match self {
            Stdio::Null => Ok(std::process::Stdio::null()),
            Stdio::File(path) => {
                let path = dir.map(|dir| dir.join(path)).unwrap_or_else(|| path.clone());
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .append(true)
                    .create(true)
                    .open(&path)
                    .with_context(|| format!("failed to open: {}", path.display()))?;
                Ok(file.into())
            }
            Stdio::Inherit => Ok(std::process::Stdio::inherit()),
        }
    }
}

impl Default for Stdio {
    fn default() -> Self {
        Self::Null
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RunCommand {
    #[serde(default)]
    pub vars: Vec<(OsString, OsString)>,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<OsString>,
    #[serde(default)]
    pub stdin: Stdio,
    #[serde(default)]
    pub stdout: Stdio,
    #[serde(default)]
    pub stderr: Stdio,
    #[serde(default)]
    pub timeout: Option<std::time::Duration>,
    #[serde(default)]
    pub current_dir: Option<PathBuf>,
}

impl RunCommand {
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            args: vec![],
            vars: vec![],
            stdin: Stdio::default(),
            timeout: None,
            stdout: Stdio::default(),
            stderr: Stdio::default(),
            current_dir: None,
        }
    }

    pub fn from_cmd_string(cmd: &str) -> Option<Self> {
        let (vars, bin, args) = crate::utils::split_command(&cmd)?;
        Some(
            Self::new(bin.into())
                .args(args.into_iter().map(|x| x.into()).collect())
                .vars(vars.into_iter().map(|(k, v)| (k.into(), v.into())).collect()),
        )
    }

    pub fn args(mut self, args: Vec<OsString>) -> Self {
        self.args = args;
        self
    }

    pub fn vars(mut self, vars: Vec<(OsString, OsString)>) -> Self {
        self.vars = vars;
        self
    }

    pub fn stdin(mut self, stdin: Stdio) -> Self {
        self.stdin = stdin;
        self
    }

    pub fn stdout(mut self, stdout: Stdio) -> Self {
        self.stdout = stdout;
        self
    }

    pub fn stderr(mut self, stderr: Stdio) -> Self {
        self.stderr = stderr;
        self
    }

    pub fn current_dir(mut self, current_dir: PathBuf) -> Self {
        self.current_dir = Some(current_dir);
        self
    }

    pub fn run(&self) -> anyhow::Result<RunOutput> {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        command.envs(self.vars.iter().cloned());
        command.stdin(self.stdin.to_stdio(self.current_dir.as_deref())?);

        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }

        command::run_command(command, self.timeout)
            .with_context(|| format!("failed to run {}", self.program.display()))
    }

    pub fn spawn(&self) -> anyhow::Result<std::process::Child> {
        let child = self
            .get_command()?
            .spawn()
            .with_context(|| format!("failed to start {}", self.program.display()))?;
        Ok(child)
    }

    pub fn get_command(&self) -> anyhow::Result<std::process::Command> {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        command.envs(self.vars.iter().cloned());

        command.stdin(self.stdin.to_stdio(self.current_dir.as_deref())?);
        command.stdout(self.stdout.to_stdio(self.current_dir.as_deref())?);
        command.stderr(self.stderr.to_stdio(self.current_dir.as_deref())?);

        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }

        Ok(command)
    }

    pub fn bash_string(&self) -> Option<String> {
        use std::fmt::Write;

        let mut command = String::new();

        for (key, value) in &self.vars {
            write!(&mut command, "{}='{}' ", key.to_str()?, value.to_str()?).ok()?
        }
        write!(&mut command, "'{}' ", self.program.display()).ok()?;
        for arg in &self.args {
            write!(&mut command, "'{}' ", arg.to_str()?).ok()?;
        }

        command.truncate(command.trim_end().len());
        Some(command)
    }
}

impl std::fmt::Display for RunCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.bash_string() {
            Some(str) => f.write_str(&str),
            None => write!(f, "{:?}", self),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum ExitKind {
    Success,
    Exit(i32),
    Crash,
    Hang,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct RunOutput {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
