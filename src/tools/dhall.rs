use crate::environment::AppConfig;
use regex::Regex;
use std::{fs, path::Path};

/// Reads and parses a Dhall configuration file into an `AppConfig` struct.
///
/// This function attempts to read a Dhall configuration from the provided file path
/// and then parse it into the `AppConfig` type. If any error occurs during reading
/// or parsing, it returns an error message as a `String`.
///
/// # Arguments
///
/// * `config_path` - A string slice representing the path to the Dhall configuration file.
///
/// # Returns
///
/// * `Ok(AppConfig)` if the configuration is successfully read and parsed.
/// * `Err(String)` if there's any error during reading or parsing, containing a descriptive error message.
///
/// # Example
///
/// ```no_run
/// use gtfs_routes_service::tools::dhall::read_dhall_config;
/// let config_path = "/path/to/config.dhall";
/// match read_dhall_config(config_path) {
///     Ok(config) => println!("Successfully read config: {:?}", config),
///     Err(err) => eprintln!("Failed to read config: {}", err),
/// }
/// ```
pub fn read_dhall_config(config_path: &str) -> Result<AppConfig, String> {
    let config = serde_dhall::from_file(config_path).parse::<AppConfig>();
    match config {
        Ok(config) => Ok(config),
        Err(e) => Err(format!("Error reading config: {}", e)),
    }
}

fn import_paths(config_text: &str) -> Vec<String> {
    let re = Regex::new(r"(?:\./|\.\./)[A-Za-z0-9_\-./]+\.dhall").expect("valid regex");
    let mut paths: Vec<String> = re
        .find_iter(config_text)
        .map(|m| m.as_str().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Decrypts `"kms:..."` literals in the dhall files the config imports.
///
/// The secrets files are imported by the config, so `serde_dhall` evaluates them
/// before the app sees any value; a `kms:` password interpolated into a URL
/// would break it. Each imported file is rewritten in place with the plaintext
/// literals. Files without a `kms:` literal are left untouched and make no AWS call.
pub async fn decrypt_dhall_imports(config_path: &str) -> Result<(), String> {
    let Ok(config_text) = fs::read_to_string(config_path) else {
        return Ok(());
    };
    let dir = Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    for import in import_paths(&config_text) {
        let path = dir.join(&import);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if !text.contains("\"kms:") {
            continue;
        }
        let decrypted = shared::tools::kms::decrypt_kms_in_dhall_text(&text)
            .await
            .map_err(|e| format!("Failed to decrypt {}: {}", import, e))?;
        fs::write(&path, decrypted)
            .map_err(|e| format!("Failed to write decrypted {}: {}", import, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_relative_dhall_imports() {
        let text =
            "let a = ./one-secrets.dhall\nlet b = ../x/two.dhall\nlet c = ./one-secrets.dhall\n{=}";
        assert_eq!(
            import_paths(text),
            vec![
                "../x/two.dhall".to_string(),
                "./one-secrets.dhall".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn plain_secrets_file_is_left_untouched() {
        let dir = std::env::temp_dir().join(format!("dhall-kms-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let secrets = "{ pass = \"plain\" }";
        fs::write(dir.join("s.dhall"), secrets).unwrap();
        fs::write(dir.join("main.dhall"), "let s = ./s.dhall\nin s").unwrap();

        let out = decrypt_dhall_imports(dir.join("main.dhall").to_str().unwrap()).await;

        assert!(out.is_ok());
        assert_eq!(fs::read_to_string(dir.join("s.dhall")).unwrap(), secrets);
        fs::remove_dir_all(&dir).unwrap();
    }
}
