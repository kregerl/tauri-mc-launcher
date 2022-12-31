use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use log::{debug, error, info, warn};
use tauri::{AppHandle, Manager, State, Wry};
use zip::ZipArchive;

use crate::{
    consts::{JAVA_VERSION_MANIFEST, LAUNCHER_NAME, LAUNCHER_VERSION},
    state::{
        account_manager::Account,
        resource_manager::{InstanceConfiguration, ManifestError, ManifestResult, ResourceState},
    },
    web_services::{
        downloader::{
            buffered_stream_download, download_bytes_from_url, download_json_object, validate_hash,
            DownloadError, Downloadable,
        },
        manifest::vanilla::{
            Argument, AssetObject, DownloadableClassifier, JavaRuntimeFile, JavaRuntimeManifest,
            JavaRuntimeType, VanillaVersion,
        },
    },
};

use super::{
    downloader::validate_file_hash,
    manifest::vanilla::{
        AssetIndex, DownloadMetadata, JarType, JavaManifest, JavaRuntime, LaunchArguments, Library,
        Logging, Rule, RuleType, VanillaManifestVersion,
    },
};

/// Checks if a single rule matches every case.
/// Returns true when an allow rule matches or a disallow rule does not match.
fn rule_matches(rule: &Rule) -> bool {
    let rule_type = &rule.rule_type;
    if rule_type.is_none() {
        return match rule.action.as_str() {
            "allow" => true,
            "disallow" => false,
            _ => unimplemented!("Unknwon rule action: {}", rule.action),
        };
    }
    match rule_type.as_ref().unwrap() {
        RuleType::Features(_feature_rules) => {
            error!("Implement feature rules for arguments");
            // FIXME: Currently just skipping these
            false
        }
        RuleType::OperatingSystem(os_rules) => {
            // Check if all the rules match the current system.
            let mut rule_matches = false;
            for (key, value) in os_rules {
                match key.as_str() {
                    "name" => {
                        let os_type = env::consts::OS;
                        if value == os_type || (os_type == "macos" && value == "osx") {
                            rule_matches = true;
                        }
                    }
                    "arch" => {
                        let os_arch = env::consts::ARCH;
                        if value == os_arch || (value == "x86" && os_arch == "x86_64") {
                            rule_matches = true;
                        }
                    }
                    "version" => { /*TODO: Check version of os to make sure it matches*/ }
                    _ => unimplemented!("Unknown rule map key: {}", key),
                }
            }
            // Check if we allow or disallow this downloadable
            match rule.action.as_str() {
                "allow" => rule_matches,
                "disallow" => !rule_matches,
                _ => unimplemented!("Unknwon rule action: {}", rule.action),
            }
        }
    }
}

fn rules_match(rules: &[Rule]) -> bool {
    let mut result = false;
    for rule in rules {
        if rule_matches(rule) {
            result = true;
        } else {
            return false;
        }
    }
    result
}

// HACK: This key generation to get the java version is not optimal and could
//       use to be redone. This uses architecture to map to known java manifest versions.
//       If the manifest ever changes this function most likely needs to be updated.
fn determine_key_for_java_manifest<'a>(
    java_version_manifest_map: &HashMap<String, JavaManifest>,
) -> &'a str {
    let os = env::consts::OS;
    let key = if os == "macos" { "mac-os" } else { os };

    if java_version_manifest_map.contains_key(key) {
        return key;
    }
    let architecture = env::consts::ARCH;
    match key {
        "linux" => {
            if architecture == "x86" {
                "linux-i386"
            } else {
                key
            }
        }
        "mac-os" => {
            if architecture == "arm" {
                "mac-os-arm64"
            } else {
                key
            }
        }
        "windows" => {
            if architecture == "x86" {
                "windows-x86"
            } else if architecture == "x86_64" {
                "windows-x64"
            } else {
                unreachable!("Unexpected windows architecture: {}", architecture)
            }
        }
        _ => {
            unreachable!(
                "Unknown java version os: {}. Expected `linux`, `mac-os` or `windows`",
                key
            )
        }
    }
}
struct LaunchArgumentPaths {
    logging: (String, PathBuf),
    library_paths: Vec<PathBuf>,
    instance_path: PathBuf,
    jar_path: PathBuf,
    asset_dir_path: PathBuf,
}

fn construct_arguments(
    main_class: String,
    arguments: &LaunchArguments,
    mc_version: &VanillaManifestVersion,
    asset_index: &str,
    argument_paths: LaunchArgumentPaths,
) -> Vec<String> {
    // Vec could be 'with_capacity' if we calculate capacity first.
    let mut formatted_arguments: Vec<String> = Vec::new();

    // Substitute values in for placeholders in the jvm arguments.
    for jvm_arg in arguments.jvm.iter() {
        match jvm_arg {
            // For normal arguments, check if it has something that should be replaced and replace it
            Argument::Arg(value) => {
                let sub_arg = substitute_jvm_arguments(&value, &argument_paths);
                formatted_arguments.push(match sub_arg {
                    Some(argument) => argument,
                    None => value.into(),
                });
            }
            // For conditional args, check their rules before adding to formatted_arguments vec
            Argument::ConditionalArg { rules, values } => {
                if !rules_match(&rules) {
                    continue;
                }
                for value in values {
                    let sub_arg = substitute_jvm_arguments(&value, &argument_paths);
                    formatted_arguments.push(match sub_arg {
                        Some(argument) => argument,
                        None => value.into(),
                    });
                }
            }
        }
    }
    // Construct the logging configuration argument
    if let Some(substr) = get_arg_substring(&argument_paths.logging.0) {
        formatted_arguments.push(
            argument_paths
                .logging
                .0
                .replace(substr, path_to_utf8_str(&argument_paths.logging.1)),
        );
    }
    // Add main class
    formatted_arguments.push(main_class);

    // Substitute values in for placeholders in the game arguments, skipping account-specific arguments.
    for game_arg in arguments.game.iter() {
        match game_arg {
            // For normal arguments, check if it has something that should be replaced and replace it
            Argument::Arg(value) => {
                let sub_arg =
                    substitute_game_arguments(&value, &mc_version, asset_index, &argument_paths);
                formatted_arguments.push(match sub_arg {
                    Some(argument) => argument,
                    None => value.into(),
                });
            }
            // For conditional args, check their rules before adding to formatted_arguments vec
            Argument::ConditionalArg { rules, values } => {
                if !rules_match(&rules) {
                    continue;
                }
                for value in values {
                    let sub_arg = substitute_game_arguments(
                        &value,
                        &mc_version,
                        asset_index,
                        &argument_paths,
                    );
                    formatted_arguments.push(match sub_arg {
                        Some(argument) => argument,
                        None => value.into(),
                    });
                }
            }
        }
    }
    println!("HERE: {:#?}", formatted_arguments);
    formatted_arguments
}

// Returns the substring inside the argument if it exists, otherwise None
fn get_arg_substring(arg: &str) -> Option<&str> {
    let substr_start = arg.chars().position(|c| c == '$');
    let substr_end = arg.chars().position(|c| c == '}');

    if let (Some(start), Some(end)) = (substr_start, substr_end) {
        Some(&arg[start..=end])
    } else {
        None
    }
}

// Returns a string with the substituted value in the jvm argument or None if it doesn't apply.
fn substitute_jvm_arguments(arg: &str, argument_paths: &LaunchArgumentPaths) -> Option<String> {
    let substring = get_arg_substring(arg);
    let classpath_strs: Vec<&str> = (&argument_paths.library_paths)
        .into_iter()
        .map(|path| path_to_utf8_str(&path))
        .collect();

    if let Some(substr) = substring {
        info!("Substituting {}", &substr);
        match substr {
            "${natives_directory}" => Some(arg.replace(
                substr,
                &format!(
                    "{}",
                    path_to_utf8_str(&argument_paths.instance_path.join("natives"))
                ),
            )),
            "${launcher_name}" => Some(arg.replace(substr, LAUNCHER_NAME)),
            "${launcher_version}" => Some(arg.replace(substr, LAUNCHER_VERSION)),
            // FIXME: Linux and Windows use different classpath serperators. Windows uses ';' and Linux uses ':'
            "${classpath}" => {
                debug!("Vec: {:#?}", classpath_strs);
                debug!("Classpath: {} ", classpath_strs.join(":"));
                Some(arg.replace(
                    substr,
                    &format!(
                        "{}:{}",
                        classpath_strs.join(":"),
                        path_to_utf8_str(&argument_paths.jar_path)
                    ),
                ))
            }
            _ => None,
        }
    } else {
        None
    }
}

fn substitute_game_arguments(
    arg: &str,
    mc_version: &VanillaManifestVersion,
    asset_index: &str,
    argument_paths: &LaunchArgumentPaths,
) -> Option<String> {
    let substring = get_arg_substring(arg);

    if let Some(substr) = substring {
        info!("Substituting {}", &substr);
        match substr {
            "${version_name}" => Some(arg.replace(substr, &mc_version.id)),
            "${game_directory}" => Some(arg.replace(
                substr,
                &format!("{}", path_to_utf8_str(&argument_paths.instance_path)),
            )),
            "${assets_root}" => Some(arg.replace(
                substr,
                &format!("{}", path_to_utf8_str(&argument_paths.asset_dir_path)),
            )),
            "${assets_index_name}" => Some(arg.replace(substr, &asset_index)),
            "${user_type}" => Some(arg.replace(substr, "mojang")), // TODO: Unknown but hardcoded to "mojang" as thats what the gdlauncher example shows
            "${version_type}" => Some(arg.replace(substr, &mc_version.version_type)),
            "${resolution_width}" => None, // TODO: Launcher option specific
            "${resolution_height}" => None, // TODO: Launcher option specific
            _ => None,
        }
    } else {
        None
    }
}

pub fn substitute_account_specific_arguments(
    arg: &str,
    active_account: &Account,
) -> Option<String> {
    if let Some(substr) = get_arg_substring(arg) {
        match substr {
            "${auth_player_name}" => Some(arg.replace(substr, &active_account.name)),
            "${auth_uuid}" => Some(arg.replace(substr, &active_account.uuid)),
            "${auth_access_token}" => {
                Some(arg.replace(substr, &active_account.minecraft_access_token))
            }
            "${clientid}" => None,  // FIXME: Unknown
            "${auth_xuid}" => None, // FIXME: Unknown
            _ => None,
        }
    } else {
        None
    }
}

/// Converts a path into a utf8 compatible string. If the string is not utf8 compatible then
/// it is set to an obvious error str: '__INVALID_UTF8_STRING__'
fn path_to_utf8_str(path: &Path) -> &str {
    match path.to_str() {
        Some(s) => s,
        None => {
            error!(
                "Retrieved invalid utf8 string from path: {}",
                path.display()
            );
            "__INVALID_UTF8_STRING__"
        }
    }
}

#[derive(Debug)]
struct LibraryData {
    library_paths: Vec<PathBuf>,
    classifiers: Vec<DownloadableClassifier>,
}

async fn download_libraries(
    libraries_dir: &Path,
    libraries: &[Library],
) -> ManifestResult<LibraryData> {
    info!("Downloading {} libraries...", libraries.len());
    if !libraries_dir.exists() {
        fs::create_dir(&libraries_dir)?;
    }

    let start = Instant::now();
    buffered_stream_download(&libraries, &libraries_dir, |bytes, lib| {
        // FIXME: Removing file hashing makes the downloads MUCH faster. Only because of a couple slow hashes, upwards of 1s each
        if !validate_hash(&bytes, &lib.hash()) {
            let err = format!("Error downloading {}, invalid hash.", &lib.url());
            error!("{}", err);
            return Err(DownloadError::InvalidFileHashError(err));
        }
        let path = lib.path(&libraries_dir);
        let mut file = File::create(&path)?;
        file.write_all(&bytes)?;
        Ok(())
    })
    .await?;
    info!(
        "Successfully downloaded libraries in {}ms",
        start.elapsed().as_millis()
    );
    let mut file_paths: Vec<PathBuf> = Vec::with_capacity(libraries.len());

    let mut classifiers: Vec<DownloadableClassifier> = Vec::new();
    for lib in libraries {
        // If there is some key for classifiers, then add them to the classifier list
        let key = lib.determine_key_for_classifiers();
        debug!("Got classifier key: {:#?}", key);
        if let Some(classifier_key) = key {
            // Add classifier to download list.
            match lib.get_classifier(&classifier_key) {
                Some(classifier) => classifiers.push(classifier),
                None => error!(
                    "Unknown classifier key {} for library {}",
                    classifier_key,
                    lib.name()
                ),
            }
        }
        // Append files paths to result list
        file_paths.push(lib.path(&libraries_dir));
    }

    debug!(
        "Downloading {} classifiers: {:#?}",
        classifiers.len(),
        classifiers
    );

    // Download additional native libraries from "classifiers"
    buffered_stream_download(&classifiers, &libraries_dir, |bytes, classifier| {
        if !validate_hash(&bytes, &classifier.hash()) {
            let err = format!("Error downloading {}, invalid hash.", &classifier.url());
            error!("{}", err);
            return Err(DownloadError::InvalidFileHashError(err));
        }
        let path = classifier.path(&libraries_dir);
        let mut file = File::create(&path)?;
        file.write_all(&bytes)?;
        Ok(())
    })
    .await?;
    Ok(LibraryData {
        library_paths: file_paths,
        classifiers,
    })
}

async fn download_game_jar(
    versions_dir: &Path,
    jar_type: JarType,
    download: &DownloadMetadata,
    version_id: &str,
) -> ManifestResult<PathBuf> {
    let jar_str = match jar_type {
        JarType::Client => "client",
        JarType::Server => "server",
    };
    // Create all dirs in path to file location.
    let dir_path = &versions_dir.join(version_id);
    fs::create_dir_all(dir_path)?;

    let path = dir_path.join(format!("{}.jar", &jar_str));
    let valid_hash = download.hash();
    // Check if the file exists and the hash matches the download's sha1.
    if !validate_file_hash(&path, valid_hash) {
        info!("Downloading {} {} jar", version_id, jar_str);
        let bytes = download_bytes_from_url(download.url()).await?;
        if !validate_hash(&bytes, valid_hash) {
            let err = format!(
                "Error downloading {} {} jar, invalid hash.",
                version_id, jar_str
            );
            error!("{}", err);
            return Err(ManifestError::InvalidFileDownload(err));
        }
        let mut file = File::create(&path)?;
        file.write_all(&bytes)?;
    }
    Ok(path)
}

// FIXME: Use an indexmap instead of a hashmap. Complete this process in a single pass since the index map is ordered correctly.
//        The correct order is important since it will create dirs before creating files in those dirs.
async fn download_java_from_runtime_manifest(
    java_dir: &Path,
    manifest: &JavaRuntime,
) -> ManifestResult<PathBuf> {
    info!("Downloading java runtime manifset");
    let version_manifest: JavaRuntimeManifest =
        download_json_object(&manifest.manifest.url()).await?;
    let base_path = &java_dir.join(&manifest.version.name);

    let mut files: Vec<JavaRuntimeFile> = Vec::new();
    // Links is a Vec<(Path, Target)>
    let mut links: Vec<(String, String)> = Vec::new();
    // Create directories first and save the remaining.
    for jrt in version_manifest.files {
        match jrt {
            JavaRuntimeType::File(jrt_file) => files.push(jrt_file),
            JavaRuntimeType::Directory(dir) => {
                let path = &base_path.join(dir);
                fs::create_dir_all(path)?;
            }
            JavaRuntimeType::Link { path, target } => links.push((path, target)),
        }
    }

    // Next download files.
    // FIXME: Currently downloading `raw` files, switch to lzma and decompress locally.
    info!("Downloading all java files.");
    let start = Instant::now();
    buffered_stream_download(&files, &base_path, |bytes, jrt| {
        if !validate_hash(&bytes, &jrt.hash()) {
            let err = format!("Error downloading {}, invalid hash.", &jrt.url());
            error!("{}", err);
            return Err(DownloadError::InvalidFileHashError(err));
        }
        let path = jrt.path(&base_path);
        let mut file = File::create(&path)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::prelude::PermissionsExt;

            if jrt.executable {
                let mut permissions = file.metadata()?.permissions();
                permissions.set_mode(0o775);
                file.set_permissions(permissions)?;
            }
        }
        file.write_all(&bytes)?;
        Ok(())
    })
    .await?;
    info!("Downloaded java in {}ms", start.elapsed().as_millis());

    // Finally create links
    for link in links {
        let to = &base_path.join(link.0);
        if !to.exists() {
            // Cant fail since the dirs were made before
            let dir_path = to.parent().unwrap().join(link.1);
            let from = dir_path.canonicalize()?;

            if from.is_dir() {
                debug!(
                    "Creating symlink between {} and {}",
                    from.display(),
                    to.display()
                );
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::symlink;

                    // Create symlink FROM "target" TO "path"
                    symlink(from, to)?;
                }
            } else {
                debug!(
                    "Creating hard link between {} and {}",
                    from.display(),
                    to.display()
                );
                // Create hard link FROM "target" TO "path"
                fs::hard_link(from, to)?;
            }
        }
    }

    let java_path = base_path.join("bin/java");
    info!("Using java path: {:?}", java_path);
    Ok(java_path)
}

async fn download_java_version(
    java_dir: &Path,
    java_component: &str,
    _java_version: u32,
) -> ManifestResult<PathBuf> {
    info!("Downloading java version manifest");
    let java_version_manifest: HashMap<String, JavaManifest> =
        download_json_object(JAVA_VERSION_MANIFEST).await?;
    let manifest_key = determine_key_for_java_manifest(&java_version_manifest);

    let java_manifest = &java_version_manifest.get(manifest_key).unwrap();
    let runtime_opt = match java_component {
        "java-runtime-alpha" => &java_manifest.java_runtime_alpha,
        "java-runtime-beta" => &java_manifest.java_runtime_beta,
        "java-runtime-gamma" => &java_manifest.java_runtime_gamma,
        "jre-legacy" => &java_manifest.jre_legacy,
        "minecraft-java-exe" => &java_manifest.minecraft_java_exe,
        _ => unreachable!(
            "No such runtime found for java component: {}",
            &java_component
        ),
    };
    info!("Downloading runtime: {:#?}", runtime_opt);
    match runtime_opt {
        Some(runtime) => {
            // let runtime_manifest = &runtime.manifest;
            Ok(download_java_from_runtime_manifest(&java_dir, &runtime).await?)
        }
        None => {
            let s = format!("Java runtime is empty for component {}", &java_component);
            error!("{}", s);
            //TODO: New error type?
            return Err(ManifestError::VersionRetrievalError(s));
        }
    }
}

/// Downloads a logging configureation into ${app_dir}/assets/objects/<first two hash chars>/${logging_configuration.id}
async fn download_logging_configurations(
    asset_objects_dir: &Path,
    logging: &Logging,
) -> ManifestResult<(String, PathBuf)> {
    let client_logger = &logging.client;
    let first_two_chars = client_logger.file_hash().split_at(2);
    let objects_dir = &asset_objects_dir.join(first_two_chars.0);
    fs::create_dir_all(&objects_dir)?;

    let path = objects_dir.join(format!("{}", &client_logger.file_id()));
    let valid_hash = &client_logger.file_hash();

    if !validate_file_hash(&path, valid_hash) {
        info!(
            "Downloading logging configuration {}",
            client_logger.file_id()
        );
        let bytes = download_bytes_from_url(&client_logger.file_url()).await?;
        if !validate_hash(&bytes, valid_hash) {
            let err = format!(
                "Error downloading logging configuration {}, invalid hash.",
                client_logger.file_id()
            );
            error!("{}", err);
            return Err(ManifestError::InvalidFileDownload(err));
        }
        let mut file = File::create(&path)?;
        file.write_all(&bytes)?;
    }
    Ok((client_logger.argument.clone(), path))
}

//TODO: This probably needs to change a little to support "legacy" versions < 1.7
async fn download_assets(
    asset_dir: &Path,
    asset_objects_dir: &Path,
    asset_index: &AssetIndex,
) -> ManifestResult<String> {
    let metadata = &asset_index.metadata;
    let asset_object: AssetObject = download_json_object(metadata.url()).await?;
    let asset_index_dir = asset_dir.join("indexes");
    let index_bytes = download_bytes_from_url(metadata.url()).await?;
    fs::create_dir_all(&asset_index_dir)?;

    info!("Asset Index ID: {:?}", &asset_index);

    let asset_index_name = format!("{}.json", asset_index.id);
    let index_path = &asset_index_dir.join(&asset_index_name);
    let mut index_file = File::create(index_path)?;
    index_file.write_all(&index_bytes)?;
    info!("Downloading {} assets", &asset_object.objects.len());

    let start = Instant::now();

    fs::create_dir_all(&asset_objects_dir)?;

    let x = buffered_stream_download(&asset_object.objects, &asset_objects_dir, |bytes, asset| {
        if !validate_hash(&bytes, &asset.hash()) {
            let err = format!("Error downloading asset {}, invalid hash.", &asset.name());
            error!("{}", err);
            return Err(DownloadError::InvalidFileHashError(err));
        }
        fs::create_dir_all(&asset.path(&asset_objects_dir).parent().unwrap())?;

        debug!(
            "Bulk Download asset path: {:#?}",
            &asset.path(&asset_objects_dir)
        );
        let mut file = File::create(&asset.path(&asset_objects_dir))?;
        file.write_all(&bytes)?;
        Ok(())
    })
    .await;
    info!(
        "Finished downloading assets in {}ms - {:#?}",
        start.elapsed().as_millis(),
        &x
    );
    Ok(asset_index.id.clone())
}

fn extract_natives(
    instance_dir: &Path,
    libraries_dir: &Path,
    classifiers: Vec<DownloadableClassifier>,
) -> ManifestResult<()> {
    debug!("Extracting Natives");
    for classifier in classifiers {
        debug!("Classifier: {:#?}", classifier);
        let classifier_path = classifier.path(libraries_dir);
        let natives_path = instance_dir.join("natives");
        let jar_file = File::open(classifier_path)?;
        let mut archive = ZipArchive::new(jar_file)?;

        'zip: for i in 0..archive.len() {
            if let Ok(mut file) = archive.by_index(i) {
                if file.is_dir() {
                    continue;
                }
                let zip_path = match file.enclosed_name() {
                    Some(name) => name.to_owned(),
                    None => continue,
                };

                debug!("ZipArchive Path: {}", zip_path.display());
                // If the zip path starts with (or is) an excluded path, dont extract it.
                if let Some(extraction_rule) = &classifier.extraction_rule {
                    for exclusion in &extraction_rule.exclude {
                        if zip_path.starts_with(exclusion) {
                            debug!("Excluding {}", exclusion);
                            continue 'zip;
                        }
                    }
                }
                let path = natives_path.join(zip_path);
                if let Some(parent) = path.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(parent)?;
                    }
                }
                debug!("Copy from {:#?} to {:#?}", file.name(), path.display());
                let mut output_file = File::create(&path)?;
                io::copy(&mut file, &mut output_file)?;
            }
        }
    }
    Ok(())
}

pub async fn create_instance(
    selected: String,
    instance_name: String,
    app_handle: &AppHandle<Wry>,
) -> ManifestResult<()> {
    let resource_state: State<ResourceState> = app_handle
        .try_state()
        .expect("`ResourceState` should already be managed.");
    let resource_manager = resource_state.0.lock().await;
    let start = Instant::now();

    let version: VanillaVersion = resource_manager.download_vanilla_version(&selected).await?;

    let libraries: Vec<Library> = version
        .libraries
        .into_iter()
        .filter_map(|lib| {
            // If we have any rules...
            if let Some(rules) = &lib.rules {
                // and the rules dont match
                if !rules_match(&rules) {
                    // remove
                    None
                } else {
                    // Otherwise keep lib in download list
                    Some(lib)
                }
            } else {
                // Otherwise keep lib in download list
                Some(lib)
            }
        })
        .collect();

    let library_data = download_libraries(&resource_manager.libraries_dir(), &libraries).await?;

    let game_jar_path = download_game_jar(
        &resource_manager.version_dir(),
        JarType::Client,
        &version.downloads.client,
        &version.id,
    )
    .await?;

    let java_path = download_java_version(
        &resource_manager.java_dir(),
        &version.java_version.component,
        version.java_version.major_version,
    )
    .await?;

    let logging =
        download_logging_configurations(&resource_manager.asset_objects_dir(), &version.logging)
            .await?;

    let asset_index = download_assets(
        &resource_manager.assets_dir(),
        &resource_manager.asset_objects_dir(),
        &version.asset_index,
    )
    .await?;
    info!(
        "Finished download instance in {}ms",
        start.elapsed().as_millis()
    );

    let instance_dir = resource_manager.instances_dir().join(&instance_name);
    fs::create_dir_all(&instance_dir)?;

    let mc_version_manifest = resource_manager.get_vanilla_manifest_from_version(&selected);
    if mc_version_manifest.is_none() {
        warn!(
            "Could not retrieve manifest for unknown version: {}.",
            &selected
        );
    }
    let persitent_arguments = construct_arguments(
        version.main_class,
        &version.arguments,
        mc_version_manifest.unwrap(),
        &asset_index,
        LaunchArgumentPaths {
            logging,
            library_paths: library_data.library_paths,
            instance_path: instance_dir.clone(),
            jar_path: game_jar_path,
            asset_dir_path: resource_manager.assets_dir(),
        },
    );
    debug!("Persistent Arguments: {}", &persitent_arguments.join(" "));

    resource_manager.add_instance(InstanceConfiguration {
        instance_name: instance_name.into(),
        jvm_path: java_path,
        arguments: persitent_arguments,
    })?;
    debug!("After persistent args");
    extract_natives(
        &instance_dir,
        &resource_manager.libraries_dir(),
        library_data.classifiers,
    )?;
    Ok(())
}
