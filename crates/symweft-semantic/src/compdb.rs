//! Read compilation commands as data. Never execute their shell/compiler commands.
use crate::{Result, model::Input};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use symweft_index::hash;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileCommand {
    pub unit_id: String,
    pub configuration: String,
    pub root: PathBuf,
    pub build_root: PathBuf,
    pub file: PathBuf,
    pub directory: PathBuf,
    pub compiler: String,
    pub arguments: Vec<String>,
    pub command_key: String,
    pub extra_inputs: Vec<Input>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CompileDatabase {
    pub path: PathBuf,
    pub configuration: String,
    pub commands: Vec<CompileCommand>,
    pub rejected: Vec<serde_json::Value>,
}
#[derive(Deserialize)]
struct Entry {
    directory: PathBuf,
    file: PathBuf,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}
pub fn key<T: Serialize>(value: &T) -> String {
    blake_key(&serde_json::to_vec(value).expect("serializable semantic key"))
}
pub fn blake_key(bytes: &[u8]) -> String {
    hash(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn absolute(base: &Path, value: &Path) -> Result<PathBuf> {
    Ok(if value.is_absolute() {
        value.to_owned()
    } else {
        base.join(value)
    }
    .canonicalize()?)
}
fn expand(
    args: &[String],
    directory: &Path,
    inputs: &mut Vec<Input>,
    depth: u8,
) -> Result<Vec<String>> {
    if depth > 4 {
        return Err("response file nesting exceeds four".into());
    }
    let mut output = Vec::new();
    for arg in args {
        if let Some(path) = arg.strip_prefix('@') {
            let path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                directory.join(path)
            };
            if fs::metadata(&path)?.len() > 1024 * 1024 {
                return Err("response file exceeds 1 MiB".into());
            }
            let bytes = fs::read(&path)?;
            inputs.push(Input {
                path: path.to_string_lossy().into(),
                hash: hash(&bytes),
            });
            let nested = shell_words::split(std::str::from_utf8(&bytes)?)?;
            output.extend(expand(&nested, directory, inputs, depth + 1)?);
        } else {
            output.push(arg.clone());
        }
        if output.len() > 16_384 {
            return Err("compile argument limit".into());
        }
    }
    Ok(output)
}
impl CompileDatabase {
    pub fn load(root: &Path, path: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        let path = path.canonicalize()?;
        if fs::metadata(&path)?.len() > 32 * 1024 * 1024 {
            return Err("compilation database exceeds 32 MiB".into());
        }
        let entries: Vec<Entry> = serde_json::from_slice(&fs::read(&path)?)?;
        if entries.len() > 100_000 {
            return Err("compilation database entry limit".into());
        }
        let build_root = path
            .parent()
            .ok_or("compilation database has no parent")?
            .to_owned();
        let configuration = key(&(&root, &path));
        let mut commands = Vec::new();
        let mut rejected = Vec::new();
        let mut seen = HashSet::new();
        for entry in entries {
            let label = entry.file.to_string_lossy().into_owned();
            let parsed = (|| -> Result<CompileCommand> {
                let directory = absolute(&build_root, &entry.directory)?;
                let file = absolute(&directory, &entry.file)?;
                if !matches!(
                    file.extension().and_then(|s| s.to_str()),
                    Some("c" | "cc" | "cpp" | "cxx" | "C")
                ) {
                    return Err("not a C/C++ translation unit".into());
                }
                if !file.starts_with(&root) && !file.starts_with(&build_root) {
                    return Err("translation unit outside source/build roots".into());
                }
                let raw = if let Some(args) = entry.arguments {
                    args
                } else {
                    if cfg!(windows) {
                        return Err(
                            "Windows compilation databases require an arguments array".into()
                        );
                    }
                    let command = entry
                        .command
                        .as_deref()
                        .ok_or("missing arguments/command")?;
                    if command.contains('$') || command.contains('`') {
                        return Err("shell expansion is unsupported; use an arguments array".into());
                    }
                    shell_words::split(command)?
                };
                let mut extra_inputs = Vec::new();
                let mut raw = expand(&raw, &directory, &mut extra_inputs, 0)?;
                if raw.is_empty() {
                    return Err("empty command".into());
                }
                if matches!(
                    Path::new(&raw[0]).file_name().and_then(|n| n.to_str()),
                    Some("ccache" | "sccache")
                ) {
                    raw.remove(0);
                }
                if raw.is_empty() {
                    return Err("missing compiler".into());
                }
                let compiler = raw.remove(0);
                let driver = Path::new(&compiler)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if driver == "env" || driver.contains('=') {
                    return Err("env/wrapped shell commands require an explicit arguments array without environment assignments".into());
                }
                let mut arguments = Vec::new();
                let mut iter = raw.into_iter();
                while let Some(arg) = iter.next() {
                    if [
                        "-I",
                        "-isystem",
                        "-iquote",
                        "-idirafter",
                        "-include",
                        "-imacros",
                        "-isysroot",
                        "--sysroot",
                        "-target",
                        "--target",
                        "-x",
                        "-resource-dir",
                    ]
                    .contains(&arg.as_str())
                    {
                        arguments.push(arg);
                        arguments.push(iter.next().ok_or("missing compiler option argument")?);
                        continue;
                    }
                    if arg == "--" {
                        continue;
                    }
                    if matches!(arg.as_str(), "-o" | "-MF" | "-MT" | "-MQ" | "-MJ") {
                        iter.next().ok_or("missing output argument")?;
                        continue;
                    }
                    if matches!(
                        arg.as_str(),
                        "-c" | "--compile"
                            | "-S"
                            | "-E"
                            | "-MD"
                            | "-MMD"
                            | "-MP"
                            | "-MG"
                            | "-M"
                            | "-MM"
                    ) || ["-MF", "-MT", "-MQ", "-MJ", "-Wp,-MD,", "-Wp,-MMD,"]
                        .iter()
                        .any(|p| arg.starts_with(p))
                    {
                        continue;
                    }
                    if arg.starts_with("-o") && arg.len() > 2 && !arg.starts_with("-objc") {
                        continue;
                    }
                    if [
                        "-fplugin",
                        "-fmodules",
                        "-fmodule-",
                        "-include-pch",
                        "-ivfsoverlay",
                        "-vfsoverlay",
                        "-Xclang",
                        "-load",
                        "--plugin",
                    ]
                    .iter()
                    .any(|p| arg.starts_with(p))
                    {
                        return Err(
                            format!("unsupported compiler extension/cache input: {arg}").into()
                        );
                    }
                    if [";", "&&", "||", "|", ">", "<"].contains(&arg.as_str()) {
                        return Err("shell operators are not compilation arguments".into());
                    }
                    if !arg.starts_with('-')
                        && absolute(&directory, Path::new(&arg)).is_ok_and(|p| p == file)
                    {
                        continue;
                    }
                    arguments.push(arg);
                }
                if driver.contains("++") {
                    arguments.push("--driver-mode=g++".into());
                }
                arguments.push(format!("-working-directory={}", directory.display()));
                let command_key = key(&(&directory, &file, &compiler, &arguments, &extra_inputs));
                let unit_id = key(&(&configuration, &file, &command_key));
                Ok(CompileCommand {
                    unit_id,
                    configuration: configuration.clone(),
                    root: root.clone(),
                    build_root: build_root.clone(),
                    file,
                    directory,
                    compiler,
                    arguments,
                    command_key,
                    extra_inputs,
                })
            })();
            match parsed {
                Ok(command) => {
                    if seen.insert(command.unit_id.clone()) {
                        commands.push(command);
                    }
                }
                Err(error) => {
                    rejected.push(serde_json::json!({"file":label,"reason":error.to_string()}))
                }
            }
        }
        if commands.is_empty() {
            return Err(format!(
                "no supported compile commands: {}",
                serde_json::to_string(&rejected)?
            )
            .into());
        }
        commands.sort_by(|a, b| a.file.cmp(&b.file).then(a.unit_id.cmp(&b.unit_id)));
        Ok(Self {
            path,
            configuration,
            commands,
            rejected,
        })
    }
}
