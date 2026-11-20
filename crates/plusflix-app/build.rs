fn main() {
    // Debug info is required for the i-slint-backend-testing layout tests to
    // find elements (absolute positions / sizes). It bloats release binaries,
    // so enable it only in the debug profile.
    let mut config = slint_build::CompilerConfiguration::new();
    if std::env::var("PROFILE").map_or(true, |profile| profile == "debug") {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint compilation failed");

    // Embed the application icon into the Windows executable.
    #[cfg(target_family = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile()
            .expect("could not embed icon.ico into the executable");
    }
}
