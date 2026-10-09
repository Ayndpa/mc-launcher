use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::{fs, path::Path};

use crate::util::{parallel_download, DownloadItem};

/// 内嵌版本 JSON 与资源索引（Minecraft 仅保存下载清单，启动时下载）
pub const EMBEDDED_VERSION_JSON: &str = include_str!("embedded/versions/26.1.2/26.1.2.json");
pub const EMBEDDED_ASSET_INDEX: &str = include_str!("embedded/assets/indexes/30.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionJson {
    #[serde(default)]
    pub arguments: Option<Arguments>,
    #[allow(dead_code)]
    pub main_class: String,
    pub libraries: Vec<Library>,
    pub asset_index: AssetIndex,
    pub downloads: Downloads,
    pub java_version: JavaVersion,
    pub id: String,
}

#[derive(Deserialize)]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<GameArg>,
    #[serde(default)]
    pub jvm: Vec<GameArg>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum GameArg {
    Str(String),
    Obj {
        #[serde(default)]
        rules: Option<Vec<Rule>>,
        value: ArgValue,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    Str(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    #[serde(default)]
    pub downloads: Option<LibDownloads>,
    #[serde(default)]
    pub rules: Option<Vec<Rule>>,
    #[serde(default)]
    pub natives: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Deserialize)]
pub struct LibDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<std::collections::BTreeMap<String, Artifact>>,
}

#[derive(Deserialize, Clone)]
pub struct Artifact {
    #[serde(default)]
    pub path: Option<String>,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Artifact {
    pub fn rel_path(&self) -> String {
        if let Some(p) = &self.path {
            return p.clone();
        }
        // 从 URL 推导相对路径
        let url_path = self.url.split("://").nth(1).unwrap_or("");
        let mut segs: Vec<&str> = url_path.split('/').collect();
        segs.remove(0); // 去掉域名
        segs.join("/")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndex {
    pub id: String,
}

#[derive(Deserialize)]
pub struct Downloads {
    pub client: Artifact,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    pub major_version: u32,
}

#[derive(Deserialize)]
pub struct AssetIndexContent {
    pub objects: serde_json::Map<String, serde_json::Value>,
}

/// BMCLAPI 镜像
pub struct Mirrors {
    pub version_base: &'static str,
    pub client_base: &'static str,
    pub resources_base: &'static str,
    pub libraries_base: &'static str,
}

pub const BMCLAPI: Mirrors = Mirrors {
    version_base: "https://bmclapi2.bangbang93.com",
    client_base: "https://bmclapi2.bangbang93.com",
    resources_base: "https://bmclapi2.bangbang93.com/assets",
    libraries_base: "https://bmclapi2.bangbang93.com/maven",
};

/// Mojang 官方源 URL 重写为 BMCLAPI 镜像
pub fn rewrite_url(url: &str) -> String {
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

pub fn rule_allowed(rules: &Option<Vec<Rule>>) -> bool {
    match rules {
        None => true,
        Some(rules) => {
            let mut allowed = false;
            for r in rules {
                let os_ok = match &r.os {
                    None => true,
                    Some(os) => {
                        let name_ok = os.name.as_deref().map_or(true, |n| n == crate::paths::os_name());
                        let arch_ok = os.arch.as_deref().map_or(true, |a| a == crate::paths::os_arch());
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

pub fn arg_strings(arg: &GameArg, v: &mut Vec<String>) {
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

pub fn expand(template: &str, map: &dyn Fn(&str) -> String) -> String {
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

/// 解压 Java：Windows 为 zip，macOS/Linux 为 tar.gz
#[cfg(windows)]
pub fn extract_java(archive: &Path, dest: &Path) -> Result<()> {
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
pub fn extract_java(archive: &Path, dest: &Path) -> Result<()> {
    use std::io::Read;
    let file = fs::File::open(archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(dec);
    fs::create_dir_all(dest)?;
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.unpack(dest)?;
    Ok(())
}

/// 从 Adoptium 下载 JRE 并解压
pub async fn download_java(root: &Path, java_home: &Path, major: u32) -> Result<()> {
    let (os, arch) = crate::paths::adoptium_target();
    eprintln!("从 Adoptium 下载 Java {major}（{os}/{arch}）…");
    let api = format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?os={os}&architecture={arch}&image_type=jdk"
    );
    let assets: serde_json::Value = crate::util::http_json(&api).await?;
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
    Ok(())
}

/// 解压 natives jar 中的本地库到 natives 目录
pub fn extract_natives(native_jars: &[std::path::PathBuf], natives_dir: &Path) -> Result<()> {
    for jar in native_jars {
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
    Ok(())
}
