//! Developer automation: the cached release build.
//!
//! `cargo run -p xtask -- build` builds `slag`, `leak` (from `cli`), `sweep`
//! (`test262`) and `wasmtest` into `target/<profile>`, reusing a cache keyed by
//! the *content* of the working tree. Keying on the commit would miss the common
//! case here: a bisect replaces individual paths (`git checkout <rev> --
//! crates/`), so the compiled tree often matches no commit at all.
//!
//! Packages are built one at a time because they resolve `runtime` with
//! different feature sets; a single invocation would unify those features and
//! change the binaries. Cache entries are shared across package selections:
//! building only `cli` leaves any `sweep` binary already stored for the same
//! tree intact.

use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod quickjs;

/// The packages this task knows, in the order the key records them.
const KNOWN_PACKAGES: [&str; 3] = ["cli", "test262", "wasmtest"];

/// The paths whose content the compiled binaries depend on. The cache key is
/// the tree hash of exactly these, so everything else in the repository
/// (`.notes/`, `.agents/`, `docs/`, `README.md`, `scratch/`, the test262
/// fixtures) is out of it.
///
/// This exists because keying on the whole tree made every documentation commit
/// a cache miss: a commit changes `HEAD^{tree}` even when no compiled input
/// moved, so after finishing a measurement and writing its notes the next
/// `build` was a full three-minute rebuild — at the moment the previous
/// commit's binaries were wanted as an A/B control.
///
/// A **new workspace member must be added here** or its edits will not
/// invalidate the cache; `the_key_covers_every_workspace_member` checks that
/// against `cargo metadata`. A path that does not exist in the checkout is
/// skipped (a pathspec that matches nothing is a hard error).
const KEY_PATHS: [&str; 5] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo",
    "crates",
];

const USAGE: &str = "\
usage: cargo run -p xtask -- <task> [options]

tasks:
  build        cached release binaries: slag, leak, sweep, wasmtest
  quickjs      the QuickJS C facade: build, link-test, check-headers,
               check-surface (`cargo run -p xtask -- quickjs --help`)

usage: cargo run -p xtask -- build [options]

Builds release binaries into target/release, reusing a content-addressed cache
under target/build-cache (override with SLAG_BUILD_CACHE).

options:
  -p, --package <name>  build only this package (repeatable): cli, test262,
                        wasmtest. Default: all three.
                        cli -> slag, leak; test262 -> sweep; wasmtest -> wasmtest
  --debug               build the debug profile instead of release
  --release             build the release profile (default)
  --no-cache            always build; do not read or write the cache
  --force               ignore an existing cache entry and rebuild
  --print-key           print the cache key for the current tree, then exit
  -h, --help            show this help

The key is the tree hash of the paths the binaries are compiled from
(`KEY_PATHS`: `crates/` plus the root manifest, lockfile, toolchain and
`.cargo`), together with the profile and toolchain — not the whole working tree,
so a documentation commit does not invalidate it. A dirty tree gets its own
entry, so checking out single paths during a bisect still hits the right
binaries.
";

#[derive(Debug)]
pub(crate) enum Error {
    Io(std::io::Error),
    /// The command could not be run at all (as opposed to exiting non-zero).
    Spawn {
        program: String,
        arguments: String,
        message: String,
    },
    /// A step failed for a reason the caller should read, not a panic.
    Step(String),
    /// Bad arguments: reported with the usage hint and exit code 2.
    Usage(String),
    /// `-h`/`--help`: print the usage and succeed.
    Help,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "{error}"),
            Error::Spawn {
                program,
                arguments,
                message,
            } => write!(formatter, "cannot run `{program} {arguments}`: {message}"),
            Error::Step(message) => write!(formatter, "{message}"),
            Error::Usage(message) => write!(formatter, "{message}"),
            Error::Help => write!(formatter, "help"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Release,
    Debug,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Profile::Release => "release",
            Profile::Debug => "debug",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Package {
    Cli,
    Test262,
    Wasmtest,
}

impl Package {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "cli" => Some(Package::Cli),
            "test262" => Some(Package::Test262),
            "wasmtest" => Some(Package::Wasmtest),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Package::Cli => "cli",
            Package::Test262 => "test262",
            Package::Wasmtest => "wasmtest",
        }
    }

    /// The binaries this package produces, in the order they are checked.
    fn binaries(self) -> &'static [&'static str] {
        match self {
            Package::Cli => &["slag", "leak"],
            Package::Test262 => &["sweep"],
            Package::Wasmtest => &["wasmtest"],
        }
    }
}

struct Options {
    selected: Vec<Package>,
    profile: Profile,
    cache: bool,
    force: bool,
    print_key: bool,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(message)) => {
            eprintln!("xtask: {message}");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &[String]) -> Result<(), Error> {
    match arguments.first().map(String::as_str) {
        Some("build") => build(&arguments[1..]),
        Some("quickjs") => quickjs::run(&arguments[1..]),
        Some("-h" | "--help") | None => Err(Error::Help),
        Some(other) => Err(Error::Usage(format!(
            "unknown task {other:?}; the tasks are `build` and `quickjs`"
        ))),
    }
}

fn build(arguments: &[String]) -> Result<(), Error> {
    let options = options(arguments)?;
    let root = workspace_root()?;
    let profile = options.profile.name();
    let wanted = wanted_binaries(&options.selected);

    let (tree, configuration) = if options.cache || options.print_key {
        compute_key(&root, profile)?
    } else {
        (String::new(), String::new())
    };

    let head = git_text(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "no-head".to_owned());
    let head_short = git_text(&root, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "no-head".to_owned());
    let head_tree =
        git_text(&root, &["rev-parse", "HEAD^{tree}"]).unwrap_or_else(|| "none".to_owned());
    let state = if tree == head_tree { "clean" } else { "dirty" };

    // Forward slashes even on Windows: the entry path is printed, and the shell
    // version of this tool spelled it that way.
    let cache_root = match env::var_os("SLAG_BUILD_CACHE") {
        Some(directory) => directory.to_string_lossy().replace('\\', "/"),
        None => format!("{}/target/build-cache", root.display()),
    };
    let entry_name = format!("{}-{}", prefix(&tree, 16), prefix(&configuration, 8));
    let entry = PathBuf::from(format!("{cache_root}/{entry_name}"));

    if options.print_key {
        println!("tree {tree}");
        println!("config {configuration}");
        println!("entry {}", entry.display());
        println!("head {head_short} ({state})");
        println!("paths {}", KEY_PATHS.join(" "));
        return Ok(());
    }

    let describe = |kind: &str| {
        println!(
            "build-cache: {kind} {}/ {profile}, HEAD {head_short} {state}",
            prefix(&tree, 16)
        );
    };

    if options.cache && !options.force && entry.is_dir() && !incomplete(&entry, &wanted) {
        describe("hit");
        restore(&root, &entry, profile, &wanted)?;
        return Ok(());
    }

    if options.cache {
        describe("miss");
    } else {
        println!("build-cache: bypassed ({profile}, HEAD {head_short} {state})");
    }

    // cargo prints one `Compiling` line per crate and then nothing while rustc
    // works and while the linker runs, which on the release profile (thin LTO,
    // one codegen unit) is minutes of silence for a single crate. Announce each
    // package and report how long it took, so a long build is not mistaken for a
    // hung one.
    println!(
        "build: {profile}: building {}",
        options
            .selected
            .iter()
            .map(|package| package.name())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for package in &options.selected {
        let name = package.name();
        let mut cargo = Command::new("cargo");
        cargo.arg("build").arg("--locked");
        if options.profile == Profile::Release {
            cargo.arg("--release");
        }
        cargo.arg("-p").arg(name);
        println!(
            "build: {profile}: cargo build --locked{} -p {name}",
            release_flag(options.profile)
        );
        let started = Instant::now();
        let status = cargo
            .current_dir(&root)
            .status()
            .map_err(|error| Error::Spawn {
                program: "cargo".to_owned(),
                arguments: format!("build -p {name}"),
                message: error.to_string(),
            })?;
        if !status.success() {
            return Err(Error::Step(format!(
                "cargo build -p {name} failed ({status})"
            )));
        }
        println!(
            "build: {profile}: {name} done in {}s",
            started.elapsed().as_secs()
        );
    }

    for binary in &wanted {
        let path = root
            .join("target")
            .join(profile)
            .join(format!("{binary}{}", env::consts::EXE_SUFFIX));
        if !path.is_file() {
            return Err(Error::Step(format!(
                "cargo did not produce target/{profile}/{binary}{}",
                env::consts::EXE_SUFFIX
            )));
        }
    }

    if !options.cache {
        return Ok(());
    }

    // The build can move the tree (a build script writing into the source, or a
    // formatter running alongside), so the entry is named after the tree the
    // binaries were actually produced from.
    let (tree, configuration) = match compute_key(&root, profile)? {
        (after_tree, after_configuration)
            if after_tree == tree && after_configuration == configuration =>
        {
            (tree, configuration)
        }
        (after_tree, after_configuration) => {
            println!(
                "build-cache: the tree changed during the build; storing under {}",
                prefix(&after_tree, 16)
            );
            (after_tree, after_configuration)
        }
    };
    let entry = PathBuf::from(format!(
        "{cache_root}/{}-{}",
        prefix(&tree, 16),
        prefix(&configuration, 8)
    ));

    store(
        &root,
        &entry,
        profile,
        &wanted,
        &tree,
        &configuration,
        &head,
        &head_short,
        state,
    )
}

fn release_flag(profile: Profile) -> &'static str {
    match profile {
        Profile::Release => " --release",
        Profile::Debug => "",
    }
}

fn options(arguments: &[String]) -> Result<Options, Error> {
    let mut selected: Vec<Package> = Vec::new();
    let mut profile = Profile::Release;
    let mut cache = true;
    let mut force = false;
    let mut print_key = false;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "-p" | "--package" => {
                let name = arguments
                    .get(index)
                    .ok_or_else(|| Error::Usage(format!("{argument} needs a package name")))?;
                index += 1;
                selected.push(package(name)?);
            }
            _ if argument.starts_with("--package=") => {
                selected.push(package(&argument["--package=".len()..])?);
            }
            "--debug" => profile = Profile::Debug,
            "--release" => profile = Profile::Release,
            "--no-cache" => cache = false,
            "--force" => force = true,
            "--print-key" => print_key = true,
            "-h" | "--help" => return Err(Error::Help),
            other => return Err(Error::Usage(format!("unknown argument: {other}"))),
        }
    }

    if selected.is_empty() {
        selected.extend(
            KNOWN_PACKAGES
                .iter()
                .filter_map(|name| Package::parse(name)),
        );
    }

    Ok(Options {
        selected,
        profile,
        cache,
        force,
        print_key,
    })
}

fn package(name: &str) -> Result<Package, Error> {
    Package::parse(name).ok_or_else(|| {
        Error::Step(format!(
            "unknown package: {name} (expected: {})",
            KNOWN_PACKAGES.join(" ")
        ))
    })
}

/// The binaries the selected packages produce.
fn wanted_binaries(selected: &[Package]) -> Vec<&'static str> {
    selected
        .iter()
        .flat_map(|package| package.binaries().iter().copied())
        .collect()
}

/// The repository root, spelled the way the cache key records it.
///
/// The key includes this path because a built binary embeds it, so the *form* is
/// part of the key. `canonicalize` is deliberately not used: on Windows it
/// yields the `\\?\C:\...` extended-length form, which is the same directory as
/// the drive-letter, forward-slash spelling under a different key.
pub(crate) fn workspace_root() -> Result<PathBuf, Error> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::Step(format!("cannot find the workspace root from {manifest:?}")))?;
    let text = root.to_string_lossy().replace('\\', "/");
    let text = text
        .strip_prefix("//?/")
        .map_or(text.clone(), str::to_owned);
    Ok(PathBuf::from(text))
}

fn prefix(value: &str, length: usize) -> &str {
    value.get(..length).unwrap_or(value)
}

fn compute_key(root: &Path, profile: &str) -> Result<(String, String), Error> {
    let index = root.join("target").join(".build-cache-index");
    fs::create_dir_all(root.join("target"))?;

    // The index starts EMPTY, so the resulting tree holds exactly the paths
    // below. Seeding it from HEAD would drag every other path in at its
    // committed content, which puts the whole repository back in the key: a
    // documentation commit is a new `HEAD^{tree}` and would miss again.
    match fs::remove_file(&index) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Only the paths that can change a compiled binary. `git add` on a
    // throwaway index snapshots them without disturbing the real index;
    // untracked files that are not ignored are part of the key (a new source
    // file counts), and a tracked file deleted here disappears from the tree.
    // A pathspec that matches nothing is a hard error, so a path that does not
    // exist in this checkout is skipped.
    let mut added = vec!["add", "-A", "--"];
    added.extend(
        KEY_PATHS
            .iter()
            .copied()
            .filter(|path| root.join(path).exists()),
    );
    let added = git_with_index(root, &added, &index)?;
    if !added.status.success() {
        return Err(Error::Step(format!(
            "cannot snapshot the source tree:\n{}",
            stderr_of(&added)
        )));
    }
    let written = git_with_index(root, &["write-tree"], &index)?;
    if !written.status.success() {
        return Err(Error::Step("git write-tree failed".to_owned()));
    }
    let tree = String::from_utf8_lossy(&written.stdout).trim().to_owned();
    if tree.is_empty() {
        return Err(Error::Step(
            "git write-tree produced an empty hash".to_owned(),
        ));
    }

    let rustc_info = rustc_text(root, &["-vV"])?;
    let mut rustflags = env::var("RUSTFLAGS").unwrap_or_default();
    rustflags.push_str(&env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default());

    let mut submodule = "absent".to_owned();
    let test262 = root.join("test262");
    if test262.join(".git").exists() {
        let head =
            git_text(&test262, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
        let status = Command::new("git")
            .current_dir(&test262)
            .args(["--no-optional-locks", "status", "--porcelain"])
            .stdin(Stdio::null())
            .output();
        let changes = match status {
            Ok(output) => hash_stdin(root, &output.stdout)?,
            Err(_) => hash_stdin(root, b"")?,
        };
        submodule = format!("{head} {changes}");
    }

    let input = format!(
        "{rustc_info}\nprofile={profile}\npackages={}\nroot={}\nrustflags={rustflags}\nrustc={}\ntarget={}\ntest262={submodule}\n",
        KNOWN_PACKAGES.join(" "),
        root.display(),
        env::var("RUSTC").unwrap_or_default(),
        env::var("CARGO_BUILD_TARGET").unwrap_or_default(),
    );
    let configuration = hash_stdin(root, input.as_bytes())?;
    Ok((tree, configuration))
}

fn incomplete(directory: &Path, wanted: &[&str]) -> bool {
    if !directory.join("meta.txt").is_file() {
        return true;
    }
    wanted.iter().any(|binary| {
        !directory
            .join(format!("{binary}{}", env::consts::EXE_SUFFIX))
            .is_file()
    })
}

fn restore(root: &Path, entry: &Path, profile: &str, wanted: &[&str]) -> Result<(), Error> {
    let target = root.join("target").join(profile);
    fs::create_dir_all(&target)?;
    for binary in wanted {
        let name = format!("{binary}{}", env::consts::EXE_SUFFIX);
        fs::copy(entry.join(&name), target.join(&name)).map_err(|error| {
            Error::Step(format!(
                "cannot restore target/{profile}/{name} (is it running?): {error}"
            ))
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn store(
    root: &Path,
    entry: &Path,
    profile: &str,
    wanted: &[&str],
    tree: &str,
    configuration: &str,
    head: &str,
    head_short: &str,
    state: &str,
) -> Result<(), Error> {
    let mut temporary = entry.as_os_str().to_owned();
    temporary.push(format!(".tmp.{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    match fs::remove_dir_all(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir_all(&temporary)?;

    // An entry may already hold binaries this selection does not rebuild (a
    // `-p cli` build after a full one); keep them and refresh only `meta.txt`.
    if entry.is_dir() {
        for item in fs::read_dir(entry)? {
            let path = item?.path();
            if !path.is_file() || path.file_name() == Some(std::ffi::OsStr::new("meta.txt")) {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            fs::copy(&path, temporary.join(name))?;
        }
    }

    for binary in wanted {
        let name = format!("{binary}{}", env::consts::EXE_SUFFIX);
        fs::copy(
            root.join("target").join(profile).join(&name),
            temporary.join(&name),
        )
        .map_err(|error| {
            Error::Step(format!(
                "cannot copy target/{profile}/{name} into the cache: {error}"
            ))
        })?;
    }

    let mut binaries: Vec<String> = Vec::new();
    for item in fs::read_dir(&temporary)? {
        let path = item?.path();
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if !path.is_file() || name == "meta.txt" {
            continue;
        }
        let name = name
            .strip_suffix(env::consts::EXE_SUFFIX)
            .unwrap_or(name.as_str())
            .to_owned();
        binaries.push(name);
    }
    binaries.sort();
    let binaries = if binaries.is_empty() {
        String::new()
    } else {
        format!("{} ", binaries.join(" "))
    };

    let rustc_short = rustc_text(root, &["-V"])?;
    let built = iso8601_utc(SystemTime::now());
    let meta = format!(
        "tree {tree}\nconfig {configuration}\nhead {head}\nhead-short {head_short}\nstate {state}\nprofile {profile}\nrustc {rustc_short}\nbuilt {built}\nbinaries {binaries}\n"
    );
    fs::write(temporary.join("meta.txt"), meta)?;

    match fs::remove_dir_all(entry) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::rename(&temporary, entry)?;
    println!("build-cache: stored {}", entry.display());
    Ok(())
}

fn git_command(root: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(arguments)
        .stdin(Stdio::null());
    command
}

fn git_with_index(root: &Path, arguments: &[&str], index: &Path) -> Result<Output, Error> {
    git_command(root, arguments)
        .env("GIT_INDEX_FILE", index)
        .output()
        .map_err(|error| Error::Spawn {
            program: "git".to_owned(),
            arguments: arguments.join(" "),
            message: error.to_string(),
        })
}

/// Standard output of a git command, trimmed, or `None` when it fails.
fn git_text(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = git_command(root, arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned(),
    )
}

fn hash_stdin(root: &Path, input: &[u8]) -> Result<String, Error> {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Spawn {
            program: "git".to_owned(),
            arguments: "hash-object --stdin".to_owned(),
            message: error.to_string(),
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Step("cannot write to `git hash-object`".to_owned()))?;
    stdin.write_all(input)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::Step(format!(
            "cannot hash the build configuration:\n{}",
            stderr_of(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn rustc_text(root: &Path, arguments: &[&str]) -> Result<String, Error> {
    let output = Command::new("rustc")
        .current_dir(root)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Spawn {
            program: "rustc".to_owned(),
            arguments: arguments.join(" "),
            message: format!("{error} (install a Rust toolchain)"),
        })?;
    if !output.status.success() {
        return Err(Error::Step(format!(
            "rustc {} failed:\n{}",
            arguments.join(" "),
            stderr_of(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_owned())
}

pub(crate) fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_owned()
}

/// `date -u +%Y-%m-%dT%H:%M:%SZ`, without asking the host for a `date`.
fn iso8601_utc(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = (seconds / 86_400) as i64;
    let clock = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the epoch to a calendar date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{KEY_PATHS, git_text};
    use std::path::{Path, PathBuf};

    /// The workspace root from this crate's manifest directory.
    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/xtask has a grandparent")
            .to_path_buf()
    }

    /// `KEY_PATHS` is an allowlist, so a workspace member outside it would have
    /// its edits silently excluded from the cache key — the cache would serve a
    /// binary built from other sources. Every member lives under `crates/`
    /// today; this fails the moment one does not, and names the fix.
    #[test]
    fn the_key_covers_every_workspace_member() {
        let root = root();
        let output = std::process::Command::new(env!("CARGO"))
            .current_dir(&root)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .expect("cargo metadata runs");
        assert!(
            output.status.success(),
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let marker = "\"manifest_path\":\"";
        let mut seen = 0;
        let mut outside = Vec::new();
        for tail in text.split(marker).skip(1) {
            let raw = tail.split('"').next().unwrap_or_default();
            // JSON escapes the Windows separator, and the path is absolute.
            let path = raw.replace("\\\\", "\\");
            seen += 1;
            let Ok(relative) = Path::new(&path).strip_prefix(&root) else {
                outside.push(path.clone());
                continue;
            };
            let ok = KEY_PATHS
                .iter()
                .any(|covered| relative.starts_with(covered));
            if !ok {
                outside.push(relative.display().to_string());
            }
        }
        assert!(seen >= 3, "expected the workspace's members, saw {seen}");
        assert!(
            outside.is_empty(),
            "workspace members outside KEY_PATHS {KEY_PATHS:?}: {outside:?} — add \
             their root to KEY_PATHS or the cache will not notice edits to them"
        );
    }

    /// A key path that does not exist is skipped at key time, so a typo would
    /// silently narrow the key. Every declared path must exist here.
    #[test]
    fn every_key_path_exists() {
        let root = root();
        for path in KEY_PATHS {
            assert!(
                root.join(path).exists(),
                "KEY_PATHS lists {path}, which is not in this checkout"
            );
        }
    }

    /// The key tracks the source tree only: a documentation commit must not
    /// change it. This is the property the allowlist exists for, asserted
    /// against the paths themselves rather than by listing them again.
    ///
    /// A crate's *own* prose stays keyed, deliberately: `crates/` is one
    /// allowlist entry, and a crate may `include_str!` a data file next to its
    /// sources (`runtime` does, `examples` do), so narrowing further by
    /// extension would trade a rare needless rebuild for a real stale-binary
    /// hazard. The trees that change constantly are the session's notes, the
    /// skills and the probes, and those are what this pins.
    #[test]
    fn the_key_excludes_the_session_notes_and_scratch() {
        let root = root();
        let tracked = git_text(&root, &["ls-files"]).expect("git ls-files runs");
        let mut leaked = Vec::new();
        for file in tracked.lines() {
            let covered = KEY_PATHS.iter().any(|path| file.starts_with(path));
            let is_outside_source = [".notes/", ".agents/", "docs/", "scratch/", "tools/"]
                .iter()
                .any(|tree| file.starts_with(tree));
            if is_outside_source && covered {
                leaked.push(file.to_owned());
            }
        }
        assert!(
            leaked.is_empty(),
            "notes/skills/probes under a keyed path would invalidate the cache: {leaked:?}"
        );
    }
}
