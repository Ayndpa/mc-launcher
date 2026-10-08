use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

const VERSION_NAME: &str = "26.1.2";
const WORK_DIR: &str = "mc";
// Fabric 支持：BMCLAPI 镜像（国内可达）
const FABRIC_LOADER_VERSION: &str = "0.19.5";
const FABRIC_META_BASE: &str = "https://bmclapi2.bangbang93.com/fabric-meta/v2/versions/loader";
const FABRIC_MAVEN_PROXY: &str = "https://bmclapi2.bangbang93.com/maven/";

struct Mirrors {
    manifest: &'static str,
    version_base: &'static str,
    client_base: &'static str,
    resources_base: &'static str,
    libraries_base: &'static str,
}

const BMCLAPI: Mirrors = Mirrors {
    manifest: "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json",
    version_base: "https://bmclapi2.bangbang93.com",
    client_base: "https://bmclapi2.bangbang93.com",
    resources_base: "https://bmclapi2.bangbang93.com/assets",
    libraries_base: "https://bmclapi2.bangbang93.com/maven",
};

#[derive(Deserialize, serde::Serialize)]
struct Manifest {
    versions: Vec<ManifestVersion>,
}

#[derive(Deserialize, serde::Serialize)]
struct ManifestVersion {
    id: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionJson {
    #[serde(default)]
    arguments: Option<Arguments>,
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
}

impl Artifact {
    fn rel_path(&self) -> String {
        if let Some(p) = &self.path {
            return p.clone();
        }
        // 从 URL 推断路径
        let url_path = self.url.split("://").nth(1).unwrap_or("");
        let mut segs: Vec<&str> = url_path.split('/').collect();
        segs.remove(0); // 域名
        segs.join("/")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetIndex {
    id: String,
    url: String,
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

// Fabric profile（只取需要的字段）
#[derive(Deserialize)]
struct FabricProfile {
    id: String,
    #[serde(rename = "mainClass")]
    main_class: String,
    libraries: Vec<FabricLib>,
}

#[derive(Deserialize)]
struct FabricLib {
    name: String,
    #[serde(default)]
    sha1: Option<String>,
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
        .with_context(|| format!("解析 {url}"))?;
    Ok(body)
}

/// 拉取 Fabric profile JSON（继承原版 26.1.2）
async fn fetch_fabric_profile() -> Result<FabricProfile> {
    let url = format!(
        "{}/{}/{}/profile/json",
        FABRIC_META_BASE, VERSION_NAME, FABRIC_LOADER_VERSION
    );
    eprintln!("获取 Fabric profile…");
    http_json(&url).await
}

/// Fabric 库 maven 路径：net.fabricmc:fabric-loader:0.19.5 -> net/fabricmc/fabric-loader/0.19.5/fabric-loader-0.19.5.jar
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

/// 安装 Fabric：下载 loader 及其依赖库，返回解析后的 profile（供 classpath/主类复用，避免二次拉取）
async fn install_fabric(libs_dir: &Path) -> Result<FabricProfile> {
    let profile = fetch_fabric_profile().await?;
    eprintln!("Fabric {}: 准备库文件…", profile.id);
    let mut items: Vec<DownloadItem> = Vec::new();
    for lib in &profile.libraries {
        let rel = fabric_lib_rel(&lib.name)?;
        let url = format!("{FABRIC_MAVEN_PROXY}{rel}");
        let dest = libs_dir.join(&rel);
        items.push(DownloadItem { url, dest, sha1: lib.sha1.clone() });
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

/// natives classifier 里的 ${arch}：Windows/Linux 用 64/32，macOS 用 arm64/x86_64
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

/// Adoptium API 的 os / architecture 参数
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
}

/// 并行下载：reqwest + tokio + futures::buffer_unordered
async fn parallel_download(items: Vec<DownloadItem>) -> Result<()> {
    // 过滤已存在的
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
    // 动态计算并发数：基于 CPU 核数，小文件多路并发收益高，上限放宽
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let concurrency = (cpus * 16).clamp(64, 256);
    eprintln!("并行下载 {total} 个文件（并发 {concurrency}）…");

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_started = std::time::Instant::now();

    let results: Vec<(String, Result<(), String>)> = futures::stream::iter(items.into_iter().map(|it| {
        let client = client.clone();
        let done = done.clone();
        async move {
            let name = it.dest.display().to_string();
            let res = download_one(&client, &it).await;
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let speed = n as f64 / total_started.elapsed().as_secs_f64().max(0.1);
            eprint!("\r进度 {n}/{total}（{speed:.1} 个/秒）          ");
            if n == total {
                eprintln!();
            }
            (name, res)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

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
    Err(last_err)
}

async fn try_download(client: &reqwest::Client, it: &DownloadItem) -> Result<(), String> {
    let resp = client
        .get(&it.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if let Some(want) = &it.sha1 {
        let got = Sha1::digest(&bytes);
        if !hex_eq(&got, want) {
            return Err("SHA1 校验失败".into());
        }
    }
    fs::write(&it.dest, &bytes).map_err(|e| e.to_string())?;
    Ok(())
}

fn java_exe_name() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// 递归查找 java 可执行文件（深度 4），兼容：
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

/// 版本清单 24h 本地缓存（避免 BMCLAPI 429 限流）
async fn load_manifest_cached() -> Result<Manifest> {
    let cache = env::current_dir()?.join(WORK_DIR).join("version_manifest_v2.json");
    if let Ok(meta) = fs::metadata(&cache) {
        if let Ok(elapsed) = meta.modified()?.elapsed() {
            if elapsed.as_secs() < 24 * 3600 {
                if let Ok(text) = fs::read_to_string(&cache) {
                    if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                        eprintln!("使用本地缓存的版本清单");
                        return Ok(m);
                    }
                }
            }
        }
    }
    let manifest: Manifest = http_json(BMCLAPI.manifest).await?;
    let _ = fs::write(&cache, serde_json::to_string(&manifest)?);
    Ok(manifest)
}

/// 解压 Java 运行时压缩包：Windows 为 zip，macOS/Linux 为 tar.gz
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

/// 官方 URL 重写到 BMCLAPI 镜像
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
    let root = env::current_dir()?.join(WORK_DIR);
    fs::create_dir_all(&root)?;
    let versions_dir = root.join("versions");
    let libs_dir = root.join("libraries");
    let assets_dir = root.join("assets");
    let natives_dir = versions_dir.join(VERSION_NAME).join("natives");
    fs::create_dir_all(&natives_dir)?;
    eprintln!("使用 BMCLAPI 国内镜像");

    // 1. 版本清单 -> 版本 JSON（24h 缓存，避免 BMCLAPI 429）
    eprintln!("获取版本清单…");
    let manifest: Manifest = load_manifest_cached().await?;
    let entry = manifest
        .versions
        .iter()
        .find(|v| v.id == VERSION_NAME)
        .ok_or_else(|| anyhow!("官方清单中找不到版本 {VERSION_NAME}"))?;
    let version_path = versions_dir.join(VERSION_NAME).join(format!("{VERSION_NAME}.json"));
    parallel_download(vec![DownloadItem {
        url: rewrite_url(&entry.url),
        dest: version_path.clone(),
        sha1: None,
    }])
    .await?;
    let version: VersionJson = serde_json::from_str(&fs::read_to_string(&version_path)?)?;

    // 2. Fabric：下载 loader 及依赖，profile 复用（不再二次拉取）
    let fabric = match install_fabric(&libs_dir).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("Fabric 安装失败，退回原版启动: {e:#}");
            None
        }
    };

    // 3. Java：优先本地 mc/java，其次系统，最后从 Adoptium 下载
    let java_home = root.join("java");
    let mut java_exe = find_java(&java_home);
    if java_exe.is_none() {
        eprintln!("未找到本地 Java，检查系统…");
        if let Ok(out) = Command::new("java").arg("-version").stderr(Stdio::piped()).output() {
            if out.status.success() {
                java_exe = Some(PathBuf::from("java"));
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
        let pkg = assets
            .get(0)
            .and_then(|a| a.get("binary"))
            .and_then(|b| b.get("package"))
            .and_then(|p| p.get("link"))
            .and_then(|l| l.as_str())
            .ok_or_else(|| anyhow!("Adoptium API 返回异常"))?
            .to_string();
        let archive_name = if cfg!(windows) { "java.zip" } else { "java.tar.gz" };
        let archive = root.join(archive_name);
        parallel_download(vec![DownloadItem { url: pkg, dest: archive.clone(), sha1: None }]).await?;
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
    }])
    .await?;

    // 5. 收集 libraries + assets 下载列表
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
                    });
                    // natives 解压在下载后统一进行
                    native_jars.push(dest);
                }
            }
        }
    }

    // 5.5 Fabric：库加入 classpath，替换主类为 KnotClient
    let main_class = if let Some(profile) = &fabric {
        for lib in &profile.libraries {
            let rel = fabric_lib_rel(&lib.name)?;
            classpath.push(libs_dir.join(&rel).display().to_string());
        }
        profile.main_class.clone()
    } else {
        version.main_class.clone()
    };

    // 6. asset index + 资源文件
    let index_path = assets_dir.join("indexes").join(format!("{}.json", version.asset_index.id));
    parallel_download(vec![DownloadItem {
        url: rewrite_url(&version.asset_index.url),
        dest: index_path.clone(),
        sha1: None,
    }])
    .await?;
    let index: AssetIndexContent = serde_json::from_str(&fs::read_to_string(&index_path)?)?;
    eprintln!("收集资源文件（{} 个）…", index.objects.len());
    for (name, obj) in &index.objects {
        let hash = obj.get("hash").and_then(|h| h.as_str()).unwrap_or_default();
        if hash.len() < 2 {
            continue;
        }
        let url = format!("{}/{}/{}", BMCLAPI.resources_base, &hash[..2], hash);
        let dest = assets_dir.join("objects").join(&hash[..2]).join(hash);
        items.push(DownloadItem { url, dest, sha1: Some(hash.to_string()) });
        let _ = name;
    }

    // 统一并行下载
    parallel_download(items).await?;

    // 解压 natives
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

    // 7. 组装参数
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
            // 26.x 版本 JSON 用 "assets_index_name"（旧版为 "asset_index_name"，两个都支持）
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
    // macOS 上 LWJGL/GLFW 必需
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
    let status = cmd.status().context("无法启动 java 进程")?;
    Ok(status.code().unwrap_or(1))
}
