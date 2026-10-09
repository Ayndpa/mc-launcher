use std::{fs, path::Path, process::{Command, Stdio}};

/// `java` 可执行文件名
fn java_exe_name() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// 从 `java -version` 输出解析主版本号（如 "1.8.0_342" -> 8，"25.0.1" -> 25）
pub fn parse_java_major(text: &str) -> Option<u32> {
    let line = text.lines().next()?;
    let seg = line.split('"').nth(1)?;
    let first = seg.split('.').next()?;
    let n: u32 = first.parse().ok()?;
    Some(if n == 1 { seg.split('.').nth(1)?.parse().ok()? } else { n })
}

/// 查找 java 可执行文件，最多下探 4 层
/// - Windows: <dir>/bin/java.exe
/// - Linux:   <dir>/bin/java
/// - macOS:   <dir>/jdk-xx.jdk/Contents/Home/bin/java
pub fn find_java(dir: &Path) -> Option<std::path::PathBuf> {
    find_java_inner(dir, 4)
}

fn find_java_inner(dir: &Path, depth: usize) -> Option<std::path::PathBuf> {
    let direct = dir.join("bin").join(java_exe_name());
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<std::path::PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    for p in subdirs {
        if let Some(f) = find_java_inner(&p, depth - 1) {
            return Some(f);
        }
    }
    None
}

/// 检查系统 Java 版本是否满足要求
pub fn system_java_ok(required: u32) -> Option<bool> {
    let out = Command::new("java")
        .arg("-version")
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let ok = parse_java_major(&text)
        .map(|v| v >= required)
        .unwrap_or(false);
    Some(out.status.success() && ok)
}
