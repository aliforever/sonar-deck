fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");

    // Embed the app icon and version info in the Windows executable.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico").set("ProductName", "Sonar Deck").set("FileDescription", "Sonar Deck");
        res.compile().expect("failed to embed Windows resources");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
