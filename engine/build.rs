fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_log_filter.c");

    if std::env::var_os("CARGO_FEATURE_FFMPEG_SYSTEM").is_none() {
        return;
    }

    let avformat = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavformat")
        .expect("libavformat development files are required by ffmpeg-system");
    let avutil = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavutil")
        .expect("libavutil development files are required by ffmpeg-system");

    let mut build = cc::Build::new();
    build.file("src/ffmpeg_log_filter.c");
    for path in avformat.include_paths.iter().chain(avutil.include_paths.iter()) {
        build.include(path);
    }
    build.flag_if_supported("-std=c11");
    build.flag_if_supported("-pthread");
    build.compile("glimpse_ffmpeg_log_filter");

    println!("cargo:rustc-link-lib=avformat");
    println!("cargo:rustc-link-lib=avutil");
    println!("cargo:rustc-link-lib=pthread");
}
