// Two jobs: tell the app which git commit it was built from, and on Windows
// give the .exe its icon.
//
// gpui loads the window icon from the resource with ID 1 in the .exe.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_commit();

    #[cfg(windows)]
    {
        // This cfg is about the host. The target can still be another system.
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            embed_windows_icon();
        }
    }
}

/// Sets `FLINT_GIT_COMMIT` for `env!`: the short commit, or "unknown" when
/// the source is not a git checkout or git is not installed.
fn emit_git_commit() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FLINT_GIT_COMMIT={commit}");

    // HEAD changes on a checkout and packed-refs when git packs the branch
    // files away. A commit writes the branch's file under refs/heads. The
    // folder is watched and not the file: after packing the file is gone,
    // and the next commit makes it again.
    watch_git_path("HEAD");
    watch_git_path("packed-refs");
    watch_git_path("refs/heads");
}

/// A path that does not exist would make cargo run this script on every build.
fn watch_git_path(name: &str) {
    let Some(path) = git(&["rev-parse", "--git-path", name]) else {
        return;
    };
    if Path::new(&path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(windows)]
fn embed_windows_icon() {
    use std::path::PathBuf;

    const ICON: &str = "assets/app-icon/icon.png";
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("build.rs: CARGO_MANIFEST_DIR is not set");
    println!("cargo:rerun-if-changed={ICON}");

    let source = image::open(PathBuf::from(manifest_dir).join(ICON))
        .expect("build.rs: failed to open the app icon")
        .to_rgba8();
    let square = pad_to_square(&source);

    // An .ico entry is at most 256x256.
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 32, 48, 64, 128, 256] {
        let resized =
            image::imageops::resize(&square, size, size, image::imageops::FilterType::Lanczos3);
        let image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        icon_dir.add_entry(
            ico::IconDirEntry::encode(&image).expect("build.rs: failed to encode icon entry"),
        );
    }

    let out_dir = std::env::var("OUT_DIR").expect("build.rs: OUT_DIR is not set");
    let ico_path = PathBuf::from(out_dir).join("flint.ico");
    let file = std::fs::File::create(&ico_path).expect("build.rs: failed to create flint.ico");
    icon_dir
        .write(file)
        .expect("build.rs: failed to write flint.ico");

    // ProductName and FileDescription are what Task Manager and the file
    // properties show.
    let mut res = winresource::WindowsResource::new();
    res.set_icon_with_id(
        ico_path.to_str().expect("build.rs: OUT_DIR is not UTF-8"),
        "1",
    );
    res.set("ProductName", "flint");
    res.set("FileDescription", "flint");
    res.compile()
        .expect("build.rs: failed to compile windows resource");
}

/// icon.png is a few pixels off square. Resizing it as it is would stretch
/// it, so it goes onto a transparent square first.
#[cfg(windows)]
fn pad_to_square(source: &image::RgbaImage) -> image::RgbaImage {
    let side = source.width().max(source.height());
    let mut square = image::RgbaImage::new(side, side);
    let left = i64::from((side - source.width()) / 2);
    let top = i64::from((side - source.height()) / 2);
    image::imageops::replace(&mut square, source, left, top);
    square
}
