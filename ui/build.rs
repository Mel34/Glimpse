fn main() {
    println!("cargo:rerun-if-changed=src/vaapi_texture.c");

    let gtk = pkg_config::Config::new()
        .atleast_version("4.24")
        .probe("gtk4")
        .expect("GTK 4.24 or newer is required for the VAAPI DMA-BUF renderer");
    let avutil = pkg_config::Config::new()
        .probe("libavutil")
        .expect("libavutil development files are required");
    let drm = pkg_config::Config::new()
        .probe("libdrm")
        .expect("libdrm development files are required");

    let mut compiler = cc::Build::new();
    compiler.file("src/vaapi_texture.c").warnings(true).flag_if_supported("-std=c11");
    for path in gtk.include_paths.iter()
        .chain(avutil.include_paths.iter())
        .chain(drm.include_paths.iter())
    {
        compiler.include(path);
    }
    compiler.compile("glimpse_vaapi_texture");
}
