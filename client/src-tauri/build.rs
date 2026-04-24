fn main() {
    // Load .env file and pass variables to rustc as compile-time env vars
    if let Ok(content) = std::fs::read_to_string(".env") {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                // Pass the env var to rustc so option_env! can pick it up
                println!("cargo:rustc-env={}={}", key, value);
            }
        }
    }
    // Re-run build script if .env changes
    println!("cargo:rerun-if-changed=.env");

    tauri_build::build()
}
