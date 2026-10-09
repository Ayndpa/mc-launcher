use anyhow::Result;
use std::{env, path::PathBuf};

pub const VERSION_NAME: &str = "26.1.2";
pub const INSTANCES_DIR: &str = "instances";
pub const DEFAULT_INSTANCE: &str = "default";

pub fn launcher_root() -> Result<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            return Ok(dir.to_path_buf());
        }
    }
    Ok(env::current_dir()?)
}

/// MC_INSTANCE 环境变量决定实例名，默认 "default"
pub fn instance_name() -> String {
    let name = env::var("MC_INSTANCE").unwrap_or_else(|_| DEFAULT_INSTANCE.into());
    let name = name.trim().to_string();
    if name.is_empty() {
        return DEFAULT_INSTANCE.into();
    }
    name.chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

pub fn instance_root() -> Result<PathBuf> {
    Ok(launcher_root()?.join(INSTANCES_DIR).join(instance_name()))
}

pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub fn os_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "arm") {
        "arm32"
    } else {
        "unknown"
    }
}

/// natives classifier 的 ${arch} 替换：Windows/Linux 为 64/32，macOS 为 arm64/x86_64
pub fn native_arch_token() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        }
    } else if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    }
}

/// Adoptium API 所需的 os / architecture 参数
pub fn adoptium_target() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "x64"
    };
    (os, arch)
}
