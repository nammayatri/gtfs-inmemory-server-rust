use crate::environment::AppConfig;

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
/// ```rust
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
