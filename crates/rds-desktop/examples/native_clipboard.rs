//! AppKit qualification probe; uses isolated named pasteboards only.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), rds_desktop::DesktopError> {
    let cases = rds_desktop::render::native_clipboard_probe()?;
    #[cfg(all(target_os = "macos", feature = "host"))]
    let cases = cases + rds_desktop::render::native_clipboard_owner_probe().await?;
    println!("native clipboard cases passed: {cases}");
    Ok(())
}
