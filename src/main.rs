use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::Duration,
};

const VERSION_NAME: &str = "26.1.2";
// (comment)
// (comment)
const INSTANCES_DIR: &str = "instances";
const DEFAULT_INSTANCE: &str = "default";

// (comment)
fn launcher_root() -> Result<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            return Ok(dir.to_path_buf());
        }
    }
    Ok(env::current_dir()?)
}
// Fabric  MavenBMCLAPI  maven  403
const FABRIC_MAVEN_PROXY: &str = "https://maven.fabricmc.net/";

struct Mirrors {
    version_base: &'static str,
    client_base: &'static str,
    resources_base: &'static str,
    libraries_base: &'static str,
}

const BMCLAPI: Mirrors = Mirrors {
    version_base: "https://bmclapi2.bangbang93.com",
    client_base: "https://bmclapi2.bangbang93.com",
    resources_base: "https://bmclapi2.bangbang93.com/assets",
    libraries_base: "https://bmclapi2.bangbang93.com/maven",
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionJson {
    #[serde(default)]
    arguments: Option<Arguments>,
    #[allow(dead_code)]
    main_class: String,
    libraries: Vec<Library>,
    asset_index: AssetIndex,
    downloads: Downloads,
    java_version: JavaVersion,
    id: String,
}

#[derive(Deserialize)]
struct Arguments {
    #[serde(default)]
    game: Vec<GameArg>,
    #[serde(default)]
    jvm: Vec<GameArg>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GameArg {
    Str(String),
    Obj {
        #[serde(default)]
        rules: Option<Vec<Rule>>,
        value: ArgValue,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArgValue {
    Str(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
struct Rule {
    action: String,
    #[serde(default)]
    os: Option<OsRule>,
    #[serde(default)]
    features: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct OsRule {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arch: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Library {
    #[serde(default)]
    downloads: Option<LibDownloads>,
    #[serde(default)]
    rules: Option<Vec<Rule>>,
    #[serde(default)]
    natives: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct LibDownloads {
    #[serde(default)]
    artifact: Option<Artifact>,
    #[serde(default)]
    classifiers: Option<std::collections::BTreeMap<String, Artifact>>,
}

#[derive(Deserialize, Clone)]
struct Artifact {
    #[serde(default)]
    path: Option<String>,
    url: String,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

impl Artifact {
    fn rel_path(&self) -> String {
        if let Some(p) = &self.path {
            return p.clone();
        }
        // (comment)
        let url_path = self.url.split("://").nth(1).unwrap_or("");
        let mut segs: Vec<&str> = url_path.split('/').collect();
        segs.remove(0); // 
        segs.join("/")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetIndex {
    id: String,
}

#[derive(Deserialize)]
struct Downloads {
    client: Artifact,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaVersion {
    major_version: u32,
}

#[derive(Deserialize)]
struct AssetIndexContent {
    objects: serde_json::Map<String, serde_json::Value>,
}

// Fabric profile
#[derive(Deserialize, serde::Serialize)]
struct FabricProfile {
    id: String,
    #[serde(rename = "mainClass")]
    main_class: String,
    libraries: Vec<FabricLib>,
}

#[derive(Deserialize, serde::Serialize)]
struct FabricLib {
    name: String,
    #[serde(default)]
    sha1: Option<String>,
}

static PROGRESS_BAR: OnceLock<std::sync::Arc<ProgressBar>> = OnceLock::new();

/// 在进度条存在时通过它输出（挂起重绘），否则直接 eprintln
fn log_msg(msg: &str) {
    if let Some(pb) = PROGRESS_BAR.get() {
        pb.println(msg);
    } else {
        eprintln!("{msg}");
    }
}

async fn http_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let body = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json::<T>()
        .await
        .with_context(|| format!(" {url}"))?;
    Ok(body)
}

/// MC_INSTANCE  "default"
fn instance_name() -> String {
    let name = env::var("MC_INSTANCE").unwrap_or_else(|_| DEFAULT_INSTANCE.into());
    let name = name.trim().to_string();
    if name.is_empty() {
        return DEFAULT_INSTANCE.into();
    }
    // (comment)
    name.chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

// (comment)
fn instance_root() -> Result<PathBuf> {
    Ok(launcher_root()?.join(INSTANCES_DIR).join(instance_name()))
}

/// Fabric profile
const EMBEDDED_FABRIC_PROFILE: &str = include_str!("embedded/fabric/profile.json");

fn fabric_profile() -> Result<FabricProfile> {
    Ok(serde_json::from_str(EMBEDDED_FABRIC_PROFILE)?)
}

// (comment)
fn fabric_lib_rel(name: &str) -> Result<String> {
    let mut segs = name.split(':');
    let group = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let artifact = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let version = segs.next().ok_or_else(|| anyhow!("bad lib name"))?;
    let dir = group.replace('.', "/");
    Ok(format!(
        "{dir}/{artifact}/{version}/{artifact}-{version}.jar"
    ))
}

// (comment)
async fn install_fabric(libs_dir: &Path) -> Result<FabricProfile> {
    let profile = fabric_profile()?;
    eprintln!("Fabric {}: 准备库文件…", profile.id);
    let mut items: Vec<DownloadItem> = Vec::new();
    for lib in &profile.libraries {
        let rel = fabric_lib_rel(&lib.name)?;
        let url = format!("{FABRIC_MAVEN_PROXY}{rel}");
        let dest = libs_dir.join(&rel);
        items.push(DownloadItem { url, dest, sha1: lib.sha1.clone(), fallback_url: None, size: 0 });
    }
    parallel_download(items).await?;
    Ok(profile)
}

fn verify_sha1(path: &Path, want: &str) -> bool {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let hash = Sha1::digest(&data);
    hex_eq(&hash, want)
}

fn hex_eq(hash: &[u8], want: &str) -> bool {
    let got: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    got.eq_ignore_ascii_case(want)
}

fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

fn os_arch() -> &'static str {
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

/// natives classifier  ${arch}Windows/Linux  64/32macOS  arm64/x86_64
fn native_arch_token() -> &'static str {
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

/// Adoptium API  os / architecture 
fn adoptium_target() -> (&'static str, &'static str) {
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

fn rule_allowed(rules: &Option<Vec<Rule>>) -> bool {
    match rules {
        None => true,
        Some(rules) => {
            let mut allowed = false;
            for r in rules {
                let os_ok = match &r.os {
                    None => true,
                    Some(os) => {
                        let name_ok = os.name.as_deref().map_or(true, |n| n == os_name());
                        let arch_ok = os.arch.as_deref().map_or(true, |a| a == os_arch());
                        name_ok && arch_ok
                    }
                };
                let feat_ok = r.features.as_ref().map_or(true, |f| f.is_empty());
                if os_ok && feat_ok {
                    allowed = r.action == "allow";
                }
            }
            allowed
        }
    }
}

fn arg_strings(arg: &GameArg, v: &mut Vec<String>) {
    match arg {
        GameArg::Str(s) => v.push(s.clone()),
        GameArg::Obj { rules, value } => {
            if rule_allowed(rules) {
                match value {
                    ArgValue::Str(s) => v.push(s.clone()),
                    ArgValue::List(l) => v.extend(l.iter().cloned()),
                }
            }
        }
    }
}

fn expand(template: &str, map: &dyn Fn(&str) -> String) -> String {
    let mut out = template.to_string();
    for key in [
        "auth_player_name",
        "version_name",
        "game_directory",
        "assets_root",
        "assets_index_name",
        "asset_index_name",
        "auth_uuid",
        "auth_access_token",
        "auth_xuid",
        "clientid",
        "user_type",
        "version_type",
        "natives_directory",
        "launcher_name",
        "launcher_version",
        "classpath",
        "classpath_separator",
        "library_directory",
        "resolution_width",
        "resolution_height",
        "quickPlayPath",
        "quickPlaySingleplayer",
        "quickPlayMultiplayer",
        "quickPlayRealms",
    ] {
        let placeholder = format!("${{{key}}}");
        while out.contains(&placeholder) {
            out = out.replace(&placeholder, &map(key));
        }
    }
    out
}

struct DownloadItem {
    url: String,
    dest: PathBuf,
    sha1: Option<String>,
    ///  URL
    fallback_url: Option<String>,
    ///  0
    size: u64,
}

/// reqwest + tokio + futures::buffer_unordered
async fn parallel_download(items: Vec<DownloadItem>) -> Result<()> {
    // 
    let items: Vec<DownloadItem> = items
        .into_iter()
        .filter(|it| {
            if !it.dest.exists() {
                return true;
            }
            match &it.sha1 {
                Some(w) => !verify_sha1(&it.dest, w),
                None => false,
            }
        })
        .collect();
    if items.is_empty() {
        eprintln!("所有文件已存在，跳过下载");
        return Ok(());
    }
    let total = items.len();
    // (comment)
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let concurrency = (cpus * 16).clamp(64, 256);
    log_msg(&format!("并行下载 {total} 个文件（并发 {concurrency}）…"));

    // (comment)
    let total_bytes: u64 = items.iter().map(|it| it.size).sum();
    let pb = if total_bytes > 0 {
        ProgressBar::new(total_bytes)
    } else {
        // 总大小未知（如资源清单无 size）：不定长度样式
        ProgressBar::new_spinner()
    };
    let pb = pb.with_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:24.cyan/blue} {pos} {bytes_per_sec} {msg}",
        )
        .unwrap()
        .progress_chars("█░ "),
    )
    .with_message(format!("0/{total} 个文件"));
    let pb = std::sync::Arc::new(pb);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_started = std::time::Instant::now();
    PROGRESS_BAR.get_or_init(|| pb.clone());
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()?;

    let results: Vec<(String, Result<(), String>)> = futures::stream::iter(items.into_iter().map(|it| {
        let client = client.clone();
        let done = done.clone();
        let pb = pb.clone();
        async move {
            let name = it.dest.display().to_string();
            let res = download_one(&client, &it).await;
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            pb.inc(it.size);
            pb.set_message(format!("{n}/{total} 个文件"));
            (name, res)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;
    pb.finish_and_clear();

    let failed: Vec<&(String, Result<(), String>)> =
        results.iter().filter(|(_, r)| r.is_err()).collect();
    let elapsed = total_started.elapsed().as_secs_f64();
    eprintln!(
        "下载完成：{total} 个，失败 {}，耗时 {elapsed:.1}s（{:.1} 个/秒）",
        failed.len(),
        total as f64 / elapsed.max(0.1)
    );
    for (name, err) in failed.iter().take(10) {
        eprintln!("  失败: {name}: {}", err.as_ref().unwrap_err());
    }
    if !failed.is_empty() {
        bail!("有 {} 个文件下载失败", failed.len());
    }
    Ok(())
}

async fn download_one(client: &reqwest::Client, it: &DownloadItem) -> Result<(), String> {
    if let Some(parent) = it.dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut last_err = String::new();
    for attempt in 0..3 {
        match try_download(client, it).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    // (comment)
    if let Some(fallback) = &it.fallback_url {
        eprintln!("\r镜像失败，切换官方源重试: {}", it.dest.display());
        let it2 = DownloadItem {
            url: fallback.clone(),
            fallback_url: None,
            dest: it.dest.clone(),
            sha1: it.sha1.clone(),
            size: it.size,
        };
        for attempt in 0..3 {
            match try_download(client, &it2).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1)))
                            .await;
                    }
                }
            }
        }
    }
    Err(last_err)
}

async fn try_download(client: &reqwest::Client, it: &DownloadItem) -> Result<(), String> {
    let resp = client
        .get(&it.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let total = resp.content_length();
    // > 8MBHEAD  Range 
    if total.map(|n| n > 8 * 1024 * 1024).unwrap_or(false) {
        drop(resp);
        return try_download_multipart(client, it, total.unwrap()).await;
    }
    // > 1MB 
    if total.map(|n| n > 1024 * 1024).unwrap_or(false) {
        let _total = total.unwrap();
        let mut file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use std::io::Write;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            if let Some(pb) = PROGRESS_BAR.get() {
                pb.inc(chunk.len() as u64);
            }
        }
        drop(file);
        return verify_and_finish(it).await;
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if let Some(want) = &it.sha1 {
        let got = Sha1::digest(&bytes);
        if !hex_eq(&got, want) {
            return Err("SHA1 ".into());
        }
    }
    fs::write(&it.dest, &bytes).map_err(|e| e.to_string())?;
    Ok(())
}

// (comment)
async fn try_download_multipart(
    client: &reqwest::Client,
    it: &DownloadItem,
    total: u64,
) -> Result<(), String> {
    const CHUNK: u64 = 8 * 1024 * 1024;
    const CONCURRENCY: usize = 8;

    //  RangeBMCLAPI 
    let probe = client
        .get(&it.url)
        .header("Range", "bytes=0-0")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let range_ok = probe.status() == reqwest::StatusCode::PARTIAL_CONTENT
        && probe
            .headers()
            .get("content-range")
            .map(|v| v.to_str().unwrap_or_default().starts_with("bytes"))
            .unwrap_or(false);
    if !range_ok {
        //  Range
        let resp = if probe.status().is_success() {
            probe
        } else {
            drop(probe);
            client
                .get(&it.url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.to_string())?
        };
        let mut file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use std::io::Write;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            if let Some(pb) = PROGRESS_BAR.get() {
                pb.inc(chunk.len() as u64);
            }
        }
        drop(file);
        return verify_and_finish(it).await;
    }
    drop(probe);

    // 
    let file = fs::File::create(&it.dest).map_err(|e| e.to_string())?;
    file.set_len(total).map_err(|e| e.to_string())?;
    drop(file);

    // 
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + CHUNK - 1).min(total - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    let total_chunks = ranges.len();

    let got = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dest = it.dest.clone();
    let url = it.url.clone();
    // (comment)
    const CHUNK_DEADLINE: Duration = Duration::from_secs(30);

    let results: Vec<Result<(), String>> = futures::stream::iter(ranges.into_iter().enumerate())
        .map(|(idx, (s, e))| {
            let client = client.clone();
            let url = url.clone();
            let dest = dest.clone();
            let got = got.clone();
            async move {
                let mut last_err = String::new();
                for attempt in 0..2 {
                    let resp = client
                        .get(&url)
                        .header("Range", format!("bytes={s}-{e}"))
                        .timeout(CHUNK_DEADLINE)
                        .send()
                        .await
                        .and_then(|r| r.error_for_status());
                    let resp = match resp {
                        Ok(r) => r,
                        Err(e) => {
                            last_err = e.to_string();
                            continue;
                        }
                    };
                    // (comment)
                    match tokio::time::timeout(
                        CHUNK_DEADLINE,
                        stream_to_file_at(resp, &dest, s),
                    )
                    .await
                    {
                        Ok(Ok(n)) => {
                            got.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            if let Some(pb) = PROGRESS_BAR.get() {
                                pb.inc(n);
                            }
                            return Ok(());
                        }
                        Ok(Err(err)) => {
                            last_err = err;
                        }
                        Err(_) => {
                            last_err = "块超时（30s），线路过慢".into();
                        }
                    }
                    if attempt < 1 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
                Err(format!("块 {idx} ({s}-{e}): {last_err}"))
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let failed: Vec<&Result<(), String>> = results.iter().filter(|r| r.is_err()).collect();
    if !failed.is_empty() {
        return Err(format!(
            "{}/{} 块下载失败: {}",
            failed.len(),
            total_chunks,
            failed[0].as_ref().unwrap_err()
        ));
    }
    verify_and_finish(it).await
}

/// 
async fn stream_to_file_at(
    resp: reqwest::Response,
    dest: &Path,
    offset: u64,
) -> Result<u64, String> {
    use futures::StreamExt;
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    let mut n: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        n += chunk.len() as u64;
    }
    Ok(n)
}

///  SHA1 
async fn verify_and_finish(it: &DownloadItem) -> Result<(), String> {
    if let Some(want) = &it.sha1 {
        let data = tokio::task::spawn_blocking({
            let path = it.dest.clone();
            move || fs::read(&path)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let got = Sha1::digest(&data);
        if !hex_eq(&got, want) {
            return Err("SHA1 ".into());
        }
    }
    Ok(())
}

fn java_exe_name() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// 从 `java -version` 输出解析主版本号（如 "1.8.0_342" -> 8，"25.0.1" -> 25）
fn parse_java_major(text: &str) -> Option<u32> {
    let line = text.lines().next()?;
    let seg = line.split('"').nth(1)?;
    let first = seg.split('.').next()?;
    let n: u32 = first.parse().ok()?;
    Some(if n == 1 { seg.split('.').nth(1)?.parse().ok()? } else { n })
}

///  java  4
/// - Windows: <dir>/bin/java.exe
/// - Linux:   <dir>/bin/java
/// - macOS:   <dir>/jdk-xx.jdk/Contents/Home/bin/java
fn find_java(dir: &Path) -> Option<PathBuf> {
    find_java_inner(dir, 4)
}

fn find_java_inner(dir: &Path, depth: usize) -> Option<PathBuf> {
    let direct = dir.join("bin").join(java_exe_name());
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = fs::read_dir(dir)
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

// (comment)
const EMBEDDED_VERSION_JSON: &str = include_str!("embedded/versions/26.1.2/26.1.2.json");
const EMBEDDED_ASSET_INDEX: &str = include_str!("embedded/assets/indexes/30.json");

///  Java Windows  zipmacOS/Linux  tar.gz
#[cfg(windows)]
fn extract_java(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    fs::create_dir_all(dest)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let out_path = dest.join(&name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(p) = out_path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut f = fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut f)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn extract_java(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(dec);
    fs::create_dir_all(dest)?;
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.unpack(dest)?;
    Ok(())
}

///  URL  BMCLAPI 
fn rewrite_url(url: &str) -> String {
    let official: &[(&str, &str)] = &[
        ("https://piston-meta.mojang.com", BMCLAPI.version_base),
        ("https://piston-data.mojang.com", BMCLAPI.client_base),
        ("https://resources.download.minecraft.net", BMCLAPI.resources_base),
        ("https://libraries.minecraft.net", BMCLAPI.libraries_base),
    ];
    let mut out = url.to_string();
    for (from, to) in official {
        if out.starts_with(from) {
            out = out.replace(from, to);
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let code = run().await?;
    std::process::exit(code);
}

async fn run() -> Result<i32> {
    let root = instance_root()?;
    fs::create_dir_all(&root)?;
    let versions_dir = root.join("versions");
    let libs_dir = root.join("libraries");
    let assets_dir = root.join("assets");
    let natives_dir = versions_dir.join(VERSION_NAME).join("natives");
    fs::create_dir_all(&natives_dir)?;

    // 1.  JSON/client 
    let version: VersionJson = serde_json::from_str(EMBEDDED_VERSION_JSON)?;

    // (comment)
    //    Fabric 
    let fabric = install_fabric(&libs_dir).await?;

    // 3. Java mc/java Adoptium 
    let java_home = root.join("java");
    let mut java_exe = find_java(&java_home);
    // 系统 Java 仅在版本满足要求时使用（MC 26.x 需要 Java 25+，Java 8 会因新 JVM 参数直接失败）
    if java_exe.is_none() {
        if let Ok(out) = Command::new("java").arg("-version").stderr(Stdio::piped()).output() {
            let text = String::from_utf8_lossy(&out.stderr);
            let version_ok = parse_java_major(&text)
                .map(|v| v >= version.java_version.major_version)
                .unwrap_or(false);
            if out.status.success() && version_ok {
                java_exe = Some(PathBuf::from("java"));
            } else if !version_ok {
                eprintln!("系统 Java 版本过低（需要 {}），改用下载的 JRE", version.java_version.major_version);
            }
        }
    }
    if java_exe.is_none() {
        let major = version.java_version.major_version;
        let (os, arch) = adoptium_target();
        eprintln!("从 Adoptium 下载 Java {major}（{os}/{arch}）…");
        let api = format!(
            "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?os={os}&architecture={arch}&image_type=jdk"
        );
        let assets: serde_json::Value = http_json(&api).await?;
        let first = assets.get(0).ok_or_else(|| anyhow!("Adoptium API 返回异常"))?;
        let pkg = first
            .pointer("/binary/package/link")
            .and_then(|l| l.as_str())
            .ok_or_else(|| anyhow!("Adoptium API 返回异常"))?
            .to_string();
        let pkg_size = first
            .pointer("/binary/package/size")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let archive_name = if cfg!(windows) { "java.zip" } else { "java.tar.gz" };
        let archive = root.join(archive_name);
        parallel_download(vec![DownloadItem { url: pkg, dest: archive.clone(), sha1: None, fallback_url: None, size: pkg_size }]).await?;
        extract_java(&archive, &java_home)?;
        let _ = fs::remove_file(&archive);
        java_exe = find_java(&java_home);
    }
    let java_exe = java_exe.ok_or_else(|| anyhow!("无法获取 Java"))?;
    eprintln!("使用 Java: {}", java_exe.display());

    // 4. client jar
    let client_path = versions_dir.join(VERSION_NAME).join(format!("{VERSION_NAME}.jar"));
    parallel_download(vec![DownloadItem {
        url: rewrite_url(&version.downloads.client.url),
        dest: client_path.clone(),
        sha1: version.downloads.client.sha1.clone(),
        fallback_url: Some(version.downloads.client.url.clone()),
        size: version.downloads.client.size.unwrap_or(0),
    }])
    .await?;

    // 5.  libraries + assets 
    let mut classpath: Vec<String> = vec![client_path.display().to_string()];
    let mut native_jars: Vec<PathBuf> = Vec::new();
    let mut items: Vec<DownloadItem> = Vec::new();
    for lib in &version.libraries {
        if !rule_allowed(&lib.rules) {
            continue;
        }
        let Some(dls) = &lib.downloads else {
            continue;
        };
        if let Some(art) = &dls.artifact {
            let rel = art.rel_path();
            let dest = libs_dir.join(&rel);
            items.push(DownloadItem {
                url: rewrite_url(&art.url),
                dest: dest.clone(),
                sha1: art.sha1.clone(),
                fallback_url: Some(art.url.clone()),
                size: art.size.unwrap_or(0),
            });
            classpath.push(dest.display().to_string());
        }
        // natives
        if let Some(natives) = &lib.natives {
            if let Some(key) = natives.get(os_name()).map(|v| v.as_str()) {
                let key = key.replace("${arch}", native_arch_token());
                if let Some(art) = dls.classifiers.as_ref().and_then(|c| c.get(&key)) {
                    let dest = libs_dir.join(art.rel_path());
                    items.push(DownloadItem {
                        url: rewrite_url(&art.url),
                        dest: dest.clone(),
                        sha1: art.sha1.clone(),
                        fallback_url: Some(art.url.clone()),
                        size: art.size.unwrap_or(0),
                    });
                    native_jars.push(dest);
                }
            }
        }
    }

    // 5.5 Fabric classpath KnotClient
    let main_class = {
        let profile = &fabric;
        for lib in &profile.libraries {
            let rel = fabric_lib_rel(&lib.name)?;
            classpath.push(libs_dir.join(&rel).display().to_string());
        }
        profile.main_class.clone()
    };

    // 6. 
    let index: AssetIndexContent = serde_json::from_str(EMBEDDED_ASSET_INDEX)?;
    eprintln!("收集资源文件（{} 个）…", index.objects.len());
    // 索引 JSON 必须写入 assets/indexes/<id>.json，游戏按 --assetIndex 参数查找文件名映射
    let index_file = assets_dir.join("indexes").join(format!("{}.json", version.asset_index.id));
    if !index_file.exists() {
        fs::create_dir_all(index_file.parent().unwrap())?;
        fs::write(&index_file, EMBEDDED_ASSET_INDEX)?;
    }
    for (name, obj) in &index.objects {
        let hash = obj.get("hash").and_then(|h| h.as_str()).unwrap_or_default();
        if hash.len() < 2 {
            continue;
        }
        let url = format!("{}/{}/{}", BMCLAPI.resources_base, &hash[..2], hash);
        let dest = assets_dir.join("objects").join(&hash[..2]).join(hash);
        items.push(DownloadItem { url, dest, sha1: Some(hash.to_string()), fallback_url: None, size: 0 });
        let _ = name;
    }

    // 
    parallel_download(items).await?;

    //  natives
    for jar in &native_jars {
        let file = fs::File::open(jar)?;
        let mut z = zip::ZipArchive::new(file)?;
        for i in 0..z.len() {
            let mut e = z.by_index(i)?;
            let name = e.name().to_string();
            if !(name.ends_with(".dll") || name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".jnilib")) {
                continue;
            }
            if name.contains('/') {
                continue;
            }
            let out = natives_dir.join(&name);
            let mut f = fs::File::create(&out)?;
            std::io::copy(&mut e, &mut f)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&out, fs::Permissions::from_mode(0o755));
            }
        }
    }

    // 7. 
    let cp_sep = if cfg!(windows) { ";" } else { ":" };
    let classpath = classpath.join(cp_sep);
    let username = env::var("MC_USERNAME").unwrap_or_else(|_| "Player".into());
    let uuid = format!("{:032}", 0x1234u128);
    let token = "0";

    let subst = |key: &str| -> String {
        match key {
            "auth_player_name" => username.clone(),
            "version_name" => version.id.clone(),
            "game_directory" => root.display().to_string(),
            "assets_root" => assets_dir.display().to_string(),
            // 26.x  JSON  "assets_index_name" "asset_index_name"
            "assets_index_name" | "asset_index_name" => version.asset_index.id.clone(),
            "auth_uuid" => uuid.clone(),
            "auth_access_token" => token.into(),
            "auth_xuid" => "0".into(),
            "clientid" => "0".into(),
            "user_type" => "msa".into(),
            "version_type" => "release".into(),
            "natives_directory" => natives_dir.display().to_string(),
            "launcher_name" => "rust-minilauncher".into(),
            "launcher_version" => "0.1".into(),
            "classpath" => classpath.clone(),
            "classpath_separator" => cp_sep.into(),
            "library_directory" => libs_dir.display().to_string(),
            "resolution_width" => "1280".into(),
            "resolution_height" => "720".into(),
            _ => String::new(),
        }
    };

    let mut jvm_args: Vec<String> = Vec::new();
    if let Some(args) = &version.arguments {
        for a in &args.jvm {
            arg_strings(a, &mut jvm_args);
        }
    }
    let jvm_args: Vec<String> = jvm_args.iter().map(|s| expand(s, &subst)).collect();
    // macOS  LWJGL/GLFW 
    #[cfg(target_os = "macos")]
    let jvm_args: Vec<String> = {
        let mut v = jvm_args;
        v.push("-XstartOnFirstThread".into());
        v
    };

    let mut game_args: Vec<String> = Vec::new();
    if let Some(args) = &version.arguments {
        for a in &args.game {
            arg_strings(a, &mut game_args);
        }
    }
    let game_args: Vec<String> = game_args.iter().map(|s| expand(s, &subst)).collect();

    let mut cmd = Command::new(&java_exe);
    cmd.args(&jvm_args).arg(&main_class).args(&game_args);
    cmd.current_dir(&root);
    eprintln!("启动: {main_class} …");
    let status = cmd.status().context(" java ")?;
    Ok(status.code().unwrap_or(1))
}
