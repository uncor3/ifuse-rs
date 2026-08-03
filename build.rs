use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const DLL_NAME: &str = "winfsp-x64.dll";

fn main() {
    println!("cargo:rerun-if-env-changed=WINFSP_DLL_PATH");
    println!("cargo:rerun-if-env-changed=WINFSP_DLL_OUTPUT_PATH");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let Some(source) = find_winfsp_dll() else {
        println!(
            "cargo:warning=WinFsp runtime DLL was not found; install WinFsp or set WINFSP_DLL_PATH"
        );
        return;
    };
    let Some(destination_dir) = dll_output_dir() else {
        println!("cargo:warning=unable to determine output directory for {DLL_NAME}");
        return;
    };
    if let Err(error) = fs::create_dir_all(&destination_dir)
        .and_then(|_| fs::copy(&source, destination_dir.join(DLL_NAME)).map(|_| ()))
    {
        println!(
            "cargo:warning=failed to copy {} to {}: {}",
            source.display(),
            destination_dir.display(),
            error
        );
    }
}

fn find_winfsp_dll() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WINFSP_DLL_PATH").map(PathBuf::from) {
        let candidate = if path.is_dir() {
            path.join(DLL_NAME)
        } else {
            path
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    for value in ["SxsDir", "InstallDir"] {
        if let Some(root) = registry_value(value) {
            let candidate = root.join("bin").join(DLL_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let bundled = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?)
        .join("vendor/winfsp-sys/winfsp/bin")
        .join(DLL_NAME);
    bundled.is_file().then_some(bundled)
}

fn registry_value(name: &str) -> Option<PathBuf> {
    let output = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\WOW6432Node\WinFsp", "/v", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.split_once("REG_SZ")
                .map(|(_, value)| PathBuf::from(value.trim()))
        })
}

fn dll_output_dir() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WINFSP_DLL_OUTPUT_PATH") {
        return Some(PathBuf::from(path));
    }
    let out_dir = Path::new(&env::var_os("OUT_DIR")?).to_path_buf();
    out_dir.ancestors().nth(3).map(Path::to_path_buf)
}
