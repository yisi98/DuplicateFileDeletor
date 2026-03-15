use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon_path = PathBuf::from("assets").join("app-icon.ico");
    let rc_exe = find_rc_exe().expect("failed to locate rc.exe for Windows resource compilation");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR was not set"));
    let rc_script = out_dir.join("app-icon.rc");
    let res_file = out_dir.join("app-icon.res");

    let icon_literal = normalize_rc_path(&icon_path);
    fs::write(&rc_script, format!("1 ICON \"{icon_literal}\"\n"))
        .expect("failed to write temporary rc script");

    let status = Command::new(rc_exe)
        .args([
            "/nologo",
            "/fo",
            res_file
                .to_str()
                .expect("resource file path was not valid UTF-8"),
            rc_script
                .to_str()
                .expect("resource script path was not valid UTF-8"),
        ])
        .status()
        .expect("failed to invoke rc.exe");

    if !status.success() {
        panic!("rc.exe failed to compile the application icon resource");
    }

    println!(
        "cargo:rustc-link-arg-bin=duplicate-file-deletor={}",
        res_file.display()
    );
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Some(candidate) = env::var_os("WindowsSdkDir")
        .map(PathBuf::from)
        .map(|root| root.join("bin"))
        .and_then(|bin_root| newest_sdk_rc(&bin_root))
    {
        return Some(candidate);
    }

    newest_sdk_rc(Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin"))
}

fn newest_sdk_rc(bin_root: &Path) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = fs::read_dir(bin_root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    versions.reverse();

    versions
        .into_iter()
        .map(|path| path.join("x64").join("rc.exe"))
        .find(|path| path.exists())
}

fn normalize_rc_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}
