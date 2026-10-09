mod fabric;
mod game;
mod java;
mod paths;
mod util;

use anyhow::{anyhow, Context, Result};
use std::{env, fs, path::PathBuf, process::Command};

use paths::{instance_root, VERSION_NAME};
use util::{parallel_download, DownloadItem};

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

    // 1. 解析内嵌版本 JSON（下载清单）
    let version: game::VersionJson = serde_json::from_str(game::EMBEDDED_VERSION_JSON)?;

    // 2. Fabric：全内嵌，写出库文件 + fabric-api 模组
    let fabric = fabric::install_fabric(&libs_dir).await?;
    fabric::ensure_fabric_api(&root).await?;

    // 3. Java：优先本实例，其次系统 Java（版本需满足要求），否则从 Adoptium 下载
    let java_home = root.join("java");
    let mut java_exe = java::find_java(&java_home);
    if java_exe.is_none() {
        match java::system_java_ok(version.java_version.major_version) {
            Some(true) => java_exe = Some(PathBuf::from("java")),
            Some(false) => eprintln!(
                "系统 Java 版本过低（需要 {}），改用下载的 JRE",
                version.java_version.major_version
            ),
            None => {}
        }
    }
    if java_exe.is_none() {
        game::download_java(&root, &java_home, version.java_version.major_version).await?;
        java_exe = java::find_java(&java_home);
    }
    let java_exe = java_exe.ok_or_else(|| anyhow!("无法获取 Java"))?;
    eprintln!("使用 Java: {}", java_exe.display());

    // 4. client jar
    let client_path = versions_dir.join(VERSION_NAME).join(format!("{VERSION_NAME}.jar"));
    parallel_download(vec![DownloadItem {
        url: game::rewrite_url(&version.downloads.client.url),
        dest: client_path.clone(),
        sha1: version.downloads.client.sha1.clone(),
        fallback_url: Some(version.downloads.client.url.clone()),
        size: version.downloads.client.size.unwrap_or(0),
    }])
    .await?;

    // 5. 收集 libraries + assets 下载任务，构建 classpath
    let mut classpath: Vec<String> = vec![client_path.display().to_string()];
    let mut native_jars: Vec<PathBuf> = Vec::new();
    let mut items: Vec<DownloadItem> = Vec::new();
    for lib in &version.libraries {
        if !game::rule_allowed(&lib.rules) {
            continue;
        }
        let Some(dls) = &lib.downloads else {
            continue;
        };
        if let Some(art) = &dls.artifact {
            let rel = art.rel_path();
            let dest = libs_dir.join(&rel);
            items.push(DownloadItem {
                url: game::rewrite_url(&art.url),
                dest: dest.clone(),
                sha1: art.sha1.clone(),
                fallback_url: Some(art.url.clone()),
                size: art.size.unwrap_or(0),
            });
            classpath.push(dest.display().to_string());
        }
        // natives
        if let Some(natives) = &lib.natives {
            if let Some(key) = natives.get(paths::os_name()).map(|v| v.as_str()) {
                let key = key.replace("${arch}", paths::native_arch_token());
                if let Some(art) = dls.classifiers.as_ref().and_then(|c| c.get(&key)) {
                    let dest = libs_dir.join(art.rel_path());
                    items.push(DownloadItem {
                        url: game::rewrite_url(&art.url),
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

    // 5.5 Fabric classpath 与主类（KnotClient）
    let main_class = {
        for lib in &fabric.libraries {
            let rel = fabric::fabric_lib_rel(&lib.name)?;
            classpath.push(libs_dir.join(&rel).display().to_string());
        }
        fabric.main_class.clone()
    };

    // 6. 资源文件
    let index: game::AssetIndexContent = serde_json::from_str(game::EMBEDDED_ASSET_INDEX)?;
    eprintln!("收集资源文件（{} 个）…", index.objects.len());
    // 索引 JSON 必须写入 assets/indexes/<id>.json，游戏按 --assetIndex 参数查找文件名映射
    let index_file = assets_dir.join("indexes").join(format!("{}.json", version.asset_index.id));
    if !index_file.exists() {
        fs::create_dir_all(index_file.parent().unwrap())?;
        fs::write(&index_file, game::EMBEDDED_ASSET_INDEX)?;
    }
    for (_name, obj) in &index.objects {
        let hash = obj.get("hash").and_then(|h| h.as_str()).unwrap_or_default();
        if hash.len() < 2 {
            continue;
        }
        let url = format!("{}/{}/{}", game::BMCLAPI.resources_base, &hash[..2], hash);
        let dest = assets_dir.join("objects").join(&hash[..2]).join(hash);
        items.push(DownloadItem { url, dest, sha1: Some(hash.to_string()), fallback_url: None, size: 0 });
    }

    parallel_download(items).await?;

    // 6.5 解压 natives
    game::extract_natives(&native_jars, &natives_dir)?;

    // 7. 组装启动参数并启动游戏
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
            // 26.x 的版本 JSON 中 "assets_index_name" 与 "asset_index_name" 均可能出现
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
            game::arg_strings(a, &mut jvm_args);
        }
    }
    let jvm_args: Vec<String> = jvm_args.iter().map(|s| game::expand(s, &subst)).collect();
    // macOS 需要 LWJGL/GLFW 在主线程运行
    #[cfg(target_os = "macos")]
    let jvm_args: Vec<String> = {
        let mut v = jvm_args;
        v.push("-XstartOnFirstThread".into());
        v
    };

    let mut game_args: Vec<String> = Vec::new();
    if let Some(args) = &version.arguments {
        for a in &args.game {
            game::arg_strings(a, &mut game_args);
        }
    }
    let game_args: Vec<String> = game_args.iter().map(|s| game::expand(s, &subst)).collect();

    let mut cmd = Command::new(&java_exe);
    cmd.args(&jvm_args).arg(&main_class).args(&game_args);
    cmd.current_dir(&root);
    eprintln!("启动: {main_class} …");
    let status = cmd.status().context("运行 Java 进程失败")?;
    Ok(status.code().unwrap_or(1))
}
