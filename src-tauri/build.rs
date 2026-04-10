fn main() {
    #[cfg(windows)]
    ensure_ico();
    tauri_build::build()
}

#[cfg(windows)]
fn ensure_ico() {
    use std::fs;
    use std::path::Path;
    // Ensure icons dir exists and create minimal icon.ico if missing (required for Windows)
    let icons_dir = Path::new("icons");
    let ico_path = icons_dir.join("icon.ico");
    if !ico_path.exists() {
        let _ = fs::create_dir_all(icons_dir);
        let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
        let image = ico::IconImage::from_rgba_data(32, 32, {
            let mut v = vec![0u8; 32 * 32 * 4];
            for i in (0..v.len()).step_by(4) {
                v[i] = 0x4a;     // R
                v[i + 1] = 0x9c; // G
                v[i + 2] = 0x5c; // B
                v[i + 3] = 255;  // A
            }
            v
        });
        if let Ok(entry) = ico::IconDirEntry::encode(&image) {
            icon_dir.add_entry(entry);
        }
        if let Ok(f) = fs::File::create(&ico_path) {
            let _ = icon_dir.write(f);
        }
    }
}
