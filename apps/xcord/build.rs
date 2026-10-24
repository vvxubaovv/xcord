fn main() {
    #[cfg(windows)]
    {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/xcord.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(icon.to_str().expect("icon path is valid UTF-8"));
        resource.set("FileDescription", "Xcord — plugin-first SSH");
        resource.set("ProductName", "Xcord");
        resource.set("LegalCopyright", "Copyright © 2026 Xcord contributors");
        resource
            .compile()
            .expect("failed to embed the Windows application icon");
    }
}
