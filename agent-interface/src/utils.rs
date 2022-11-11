use std::path::Path;

/// Read the entries of the directory at `path` with simplified metadata.
pub fn read_dir_entries(path: &Path) -> anyhow::Result<Vec<crate::DirEntry>> {
    let mut entries = vec![];

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = match entry.metadata() {
            Ok(data) => data,
            Err(_) => continue,
        };

        entries.push(crate::DirEntry {
            path: entry.path().canonicalize()?,
            is_file: metadata.is_file(),
            len: metadata.len(),
            modified: metadata.modified().unwrap_or_else(|_| std::time::SystemTime::now()),
        });
    }

    Ok(entries)
}

/// Split a shell-like command string into three components, `vars`, `bin`, and `args`
pub fn split_command(input: &str) -> Option<(Vec<(String, String)>, String, Vec<String>)> {
    let mut input = shlex::split(input)?.into_iter().peekable();

    let mut vars = vec![];
    while let Some(pos) = input.peek().and_then(|x| x.find('=')) {
        let var = input.next().unwrap();
        let (key, value) = var.split_at(pos);
        vars.push((key.to_owned(), value[1..].to_owned()));
    }

    let bin = input.next()?;
    let args = input.collect();

    Some((vars, bin, args))
}
