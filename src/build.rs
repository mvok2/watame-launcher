fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        if std::path::Path::new("epik.ico").exists() {
            res.set_icon("epik.ico");
        }
        res.set("ProductName", "Kaizen Launcher");
        res.set("FileDescription", "Kaizen Launcher");
        res.set("LegalCopyright", "");
        let _ = res.compile();
    }
}