//! The QuickJS C facade: both mutually-exclusive skins, their artifacts, a C
//! link test, and the vendored headers' pinned checksums.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Error, stderr_of, workspace_root};

type Result<T> = std::result::Result<T, Error>;

/// The two skins. They are mutually exclusive: each produces a `libquickjs`
/// under the same target dir, so they are built one at a time.
const FEATURES: [&str; 2] = ["ng", "bellard"];

/// The artifact names cargo can give the staticlib and the shared library,
/// which follow the host toolchain rather than the crate name.
const ARTIFACTS: [&str; 6] = [
    "libquickjs.a",
    "quickjs.lib",
    "libquickjs.so",
    "libquickjs.dylib",
    "quickjs.dll",
    "libquickjs.dll",
];

/// The vendored headers are patched (MSVC, inline-refcount removal), so drift
/// means an edit that did not go through an intentional re-vendor.
const PINNED_HEADERS: [(&str, &str); 2] = [
    (
        "crates/quickjs/c/include/ng/quickjs.h",
        "6d3879c856551653a1c64fabe52d6d07dc84e97bac3df45848df626702fc30de",
    ),
    (
        "crates/quickjs/c/include/bellard/quickjs.h",
        "6904cf9ef0aa0b8ff4e9f32f8fa0e19a3971ebb1433c02a7854c8ea81083ec26",
    ),
];

/// Windows system libraries the facade's static artifact needs linked
/// alongside it. `winmm` arrived with `runtime::sampler`, which calls
/// `timeBeginPeriod`; the shell script this replaced predated that, and the
/// job only runs on Linux, so the gap was never seen.
const WINDOWS_SYSTEM_LIBS: [&str; 12] = [
    "ws2_32", "ntdll", "advapi32", "bcrypt", "userenv", "shell32", "ole32", "oleaut32", "uuid",
    "psapi", "dbghelp", "winmm",
];

const UNIX_SYSTEM_LIBS: [&str; 3] = ["pthread", "dl", "m"];

pub(crate) fn run(arguments: &[String]) -> Result<()> {
    match arguments.first().map(String::as_str) {
        Some("build") => build(),
        Some("link-test") => link_test(&arguments[1..]),
        Some("check-headers") => check_headers(),
        Some("check-surface") => check_surface(),
        Some(other) => Err(Error::Usage(format!(
            "unknown quickjs task {other:?}; expected build, link-test, check-headers or check-surface"
        ))),
        None => Err(Error::Usage(
            "quickjs needs a task: build, link-test, check-headers or check-surface".to_owned(),
        )),
    }
}

/// Build both skins and keep each build's artifacts, which would otherwise
/// overwrite each other in `target/release`, under `target/quickjs/<feature>/`.
fn build() -> Result<()> {
    let root = workspace_root()?;
    for feature in FEATURES {
        cargo_build(&root, feature)?;
        let out = root.join("target").join("quickjs").join(feature);
        fs::create_dir_all(&out)?;
        let mut copied = 0;
        for item in fs::read_dir(root.join("target").join("release"))? {
            let path = item?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !ARTIFACTS.contains(&name) {
                continue;
            }
            fs::copy(&path, out.join(name))?;
            copied += 1;
        }
        println!(
            "quickjs: {feature}: {copied} artifact(s) in {}",
            shown(&out)
        );
    }
    Ok(())
}

fn check_headers() -> Result<()> {
    let root = workspace_root()?;
    let mut drifted = false;
    for (relative, expected) in PINNED_HEADERS {
        let bytes = fs::read(root.join(relative))
            .map_err(|error| Error::Step(format!("cannot read {relative}: {error}")))?;
        let actual = hex(&sha256(&bytes));
        if actual != expected {
            eprintln!("header drift: {relative}");
            eprintln!("  expected {expected}");
            eprintln!("  actual   {actual}");
            drifted = true;
        }
    }
    if drifted {
        return Err(Error::Step(
            "a vendored QuickJS header changed; re-vendor intentionally and update the pin"
                .to_owned(),
        ));
    }
    println!("quickjs: both vendored headers match their pinned checksums");
    Ok(())
}

/// Compile the tiny C host against both the shared and the static artifacts of
/// each skin, and check that it evaluates `1 + 2`.
fn link_test(arguments: &[String]) -> Result<()> {
    let features: Vec<&str> = if arguments.is_empty() {
        FEATURES.to_vec()
    } else {
        arguments.iter().map(String::as_str).collect()
    };
    for feature in &features {
        if !FEATURES.contains(feature) {
            return Err(Error::Usage(format!(
                "unknown feature {feature:?}; expected ng or bellard"
            )));
        }
    }

    let root = workspace_root()?;
    let compiler = c_compiler()?;
    let windows = cfg!(windows);
    let staticlib = if windows {
        "quickjs.lib"
    } else {
        "libquickjs.a"
    };
    let import_library = if windows {
        Some("quickjs.dll.lib")
    } else {
        None
    };
    let shim = if windows {
        "qjs_shim.lib"
    } else {
        "libqjs_shim.a"
    };
    let system_libraries: &[&str] = if windows {
        &WINDOWS_SYSTEM_LIBS
    } else {
        &UNIX_SYSTEM_LIBS
    };

    let work = scratch_directory()?;
    let outcome = (|| {
        for feature in &features {
            cargo_build(&root, feature)?;
            let release = root.join("target").join("release");
            let header = root.join("crates/quickjs/c/include").join(feature);
            let source = root.join("crates/quickjs/c/link-test/hello.c");
            let shim_path = find_shim(&release.join("build"), shim)?;

            let shared = work.join(format!("hello_shared{}", std::env::consts::EXE_SUFFIX));
            let mut link = Command::new(&compiler);
            link.arg("-I").arg(&header).arg(&source);
            if let Some(import_library) = import_library {
                link.arg(release.join(import_library));
            } else {
                link.arg("-L").arg(&release).arg("-lquickjs");
                let mut rpath = std::ffi::OsString::from("-Wl,-rpath,");
                rpath.push(&release);
                link.arg(rpath);
            }
            link.arg("-o").arg(&shared);
            run_command(&mut link, "the shared link test")?;
            // A Windows shared binary resolves its DLL from the directory it
            // runs in, so it is run from `target/release`.
            let cwd = if windows {
                Some(release.as_path())
            } else {
                None
            };
            expect_three(&shared, cwd, &format!("{feature} shared"))?;

            let statically = work.join(format!("hello_static{}", std::env::consts::EXE_SUFFIX));
            let mut link = Command::new(&compiler);
            link.arg("-I")
                .arg(&header)
                .arg(&source)
                .arg(release.join(staticlib))
                .arg(&shim_path);
            for library in system_libraries {
                link.arg(format!("-l{library}"));
            }
            link.arg("-o").arg(&statically);
            run_command(&mut link, "the static link test")?;
            expect_three(&statically, None, &format!("{feature} static"))?;

            println!("quickjs: {feature}: shared and static link tests OK");
        }
        Ok(())
    })();

    match fs::remove_dir_all(&work) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
    outcome
}

/// The surface ledger is `tools/quickjs-surface.py`, which reads a TOML gap list
/// per skin; it stays Python rather than duplicating a TOML reader here.
fn check_surface() -> Result<()> {
    let root = workspace_root()?;
    let python = python_with_tomllib()?;
    for feature in FEATURES {
        println!("quickjs: surface ledger for {feature}");
        let status = Command::new(&python)
            .current_dir(&root)
            .args(["tools/quickjs-surface.py", feature])
            .stdin(Stdio::null())
            .status()
            .map_err(|error| Error::Spawn {
                program: python.clone(),
                arguments: format!("tools/quickjs-surface.py {feature}"),
                message: error.to_string(),
            })?;
        if !status.success() {
            return Err(Error::Step(format!(
                "the surface ledger failed for {feature} ({status})"
            )));
        }
    }
    Ok(())
}

fn shown(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn cargo_build(root: &Path, feature: &str) -> Result<()> {
    println!("quickjs: cargo build --locked -p quickjs --features {feature} --release");
    let status = Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "-p",
            "quickjs",
            "--features",
            feature,
            "--release",
        ])
        .stdin(Stdio::null())
        .status()
        .map_err(|error| Error::Spawn {
            program: "cargo".to_owned(),
            arguments: format!("build -p quickjs --features {feature}"),
            message: error.to_string(),
        })?;
    if !status.success() {
        return Err(Error::Step(format!(
            "cargo build -p quickjs --features {feature} failed ({status})"
        )));
    }
    Ok(())
}

fn run_command(command: &mut Command, label: &str) -> Result<()> {
    let status = command.status().map_err(|error| Error::Spawn {
        program: format!("{:?}", command.get_program()),
        arguments: label.to_owned(),
        message: error.to_string(),
    })?;
    if !status.success() {
        return Err(Error::Step(format!("{label} failed ({status})")));
    }
    Ok(())
}

fn expect_three(program: &Path, directory: Option<&Path>, label: &str) -> Result<()> {
    let mut command = Command::new(program);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let output = command.output().map_err(|error| Error::Spawn {
        program: program.display().to_string(),
        arguments: label.to_owned(),
        message: error.to_string(),
    })?;
    if !output.status.success() {
        return Err(Error::Step(format!(
            "{label} exited with {}:\n{}",
            output.status,
            stderr_of(&output)
        )));
    }
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if printed != "3" {
        return Err(Error::Step(format!(
            "{label} printed {printed:?}, expected \"3\""
        )));
    }
    Ok(())
}

fn c_compiler() -> Result<String> {
    for candidate in ["clang", "cc", "gcc"] {
        let works = Command::new(candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if works {
            return Ok(candidate.to_owned());
        }
    }
    Err(Error::Step(
        "no C compiler (clang/cc/gcc) found; the link test builds a C host".to_owned(),
    ))
}

fn python_with_tomllib() -> Result<String> {
    for candidate in ["python3", "python"] {
        let works = Command::new(candidate)
            .args(["-c", "import tomllib"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if works {
            return Ok(candidate.to_owned());
        }
    }
    Err(Error::Step(
        "no python with tomllib (3.11+) found; the surface ledger needs it".to_owned(),
    ))
}

/// The shim archive cargo's build script emits, preferring the one under
/// `build/quickjs-*` over any other crate's.
fn find_shim(build_dir: &Path, name: &str) -> Result<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut pending = vec![build_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Some(entries) = fs::read_dir(&directory).ok() else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().and_then(|file| file.to_str()) == Some(name) {
                matches.push(path);
            }
        }
    }
    matches.sort();
    let preferred = matches
        .iter()
        .find(|path| path.to_string_lossy().contains("quickjs"))
        .or_else(|| matches.first());
    preferred
        .cloned()
        .ok_or_else(|| Error::Step(format!("{name} not found under {}", build_dir.display())))
}

fn scratch_directory() -> Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!("slag-quickjs-link-{}", std::process::id()));
    match fs::remove_dir_all(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// SHA-256, so the header pin needs no `sha256sum` (absent on Windows) and no
/// dependency for one hash.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut schedule = [0u32; 64];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            schedule[index] = u32::from_be_bytes(*word);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}
