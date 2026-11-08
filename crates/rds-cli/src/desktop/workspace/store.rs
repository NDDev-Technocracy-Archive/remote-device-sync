//! Small, versioned workspace preferences. Grants remain external references.
use super::WorkspaceConfig;
use anyhow::Context;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{
    io::{Read, Write},
    path::Path,
};

const MAX_BYTES: usize = 64 * 1024;

pub(super) fn load(path: &Path) -> anyhow::Result<Option<WorkspaceConfig>> {
    let file = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => std::fs::File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "workspace preferences must be a regular file"
    );
    let mut bytes = vec![];
    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "workspace preferences exceed 64 KiB"
    );
    let config: WorkspaceConfig = serde_json::from_slice(&bytes)?;
    config.validate()?;
    Ok(Some(config))
}

pub(super) fn save(path: &Path, config: &WorkspaceConfig) -> anyhow::Result<()> {
    config.validate()?;
    let mut bytes = serde_json::to_vec_pretty(config)?;
    bytes.push(b'\n');
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "workspace preferences exceed 64 KiB"
    );
    let name = path
        .file_name()
        .context("workspace path needs a filename")?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    match rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => anyhow::ensure!(
            rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile
                && stat.st_nlink == 1,
            "workspace destination must be a regular file with one link"
        ),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let temporary = format!(".rds-workspace-{:032x}.tmp", rand::random::<u128>());
    let file = rustix::fs::openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut published = false;
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::File::from(file);
        file.write_all(&bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(&directory, temporary.as_str(), &directory, name)?;
        published = true;
        rustix::fs::fsync(&directory)
            .context("workspace renamed; directory sync failed, save outcome uncertain")?;
        Ok(())
    })();
    // Remove only our exclusively created name, never a pre-existing file.
    if !published {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::workspace::DeviceConfig;
    use rds_desktop::render::workspace::{TabProfile, TabSpec};

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("rds-workspace-{:032x}", rand::random::<u128>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn config() -> WorkspaceConfig {
        WorkspaceConfig {
            schema_version: 2,
            appearance: Default::default(),
            presentation: Default::default(),
            devices: vec![DeviceConfig {
                key: "alpha".into(),
                target: "alpha".into(),
                label: "Work computer".into(),
                grant_file: None,
                local_addresses: vec![],
            }],
            tabs: vec![TabSpec {
                device: "alpha".into(),
                label: "Work computer".into(),
                display: 3,
                profile: TabProfile::default(),
            }],
        }
    }

    #[test]
    fn saves_atomic_private_preferences_without_rewriting_external_profiles() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Temp::new();
        let path = dir.0.join("workspace.json");
        std::fs::write(dir.0.join("viewer.json"), b"legacy profile").unwrap();
        save(&path, &config()).unwrap();
        let mut old = std::fs::File::open(&path).unwrap();
        let mut updated = config();
        updated.appearance =
            rds_desktop::render::workspace::Appearance::parse("light-contrast").unwrap();
        updated.tabs[0].profile.max_fps = 60;
        updated.presentation = super::super::Presentation::Spaces;
        save(&path, &updated).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().appearance, updated.appearance);
        assert_eq!(load(&path).unwrap().unwrap().tabs[0].profile.max_fps, 60);
        assert!(load(&path).unwrap().unwrap().presentation == super::super::Presentation::Spaces);
        let mut bytes = vec![];
        old.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            serde_json::from_slice::<WorkspaceConfig>(&bytes)
                .unwrap()
                .tabs[0]
                .profile
                .max_fps,
            30
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(dir.0.join("viewer.json")).unwrap(),
            b"legacy profile"
        );
    }

    #[test]
    fn reading_a_version_one_workspace_preserves_its_bytes_and_explicit_save_upgrades_it() {
        let dir = Temp::new();
        let path = dir.0.join("workspace.json");
        let mut old = serde_json::to_value(config()).unwrap();
        old["schema_version"] = 1.into();
        old.as_object_mut().unwrap().remove("appearance");
        let bytes = serde_json::to_vec(&old).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.appearance.id(), "dark");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let tabs = loaded.tabs.clone();
        loaded.schema_version = 2;
        loaded.appearance =
            rds_desktop::render::workspace::Appearance::parse("dark-contrast").unwrap();
        save(&path, &loaded).unwrap();
        let restored = load(&path).unwrap().unwrap();
        assert_eq!(restored.schema_version, 2);
        assert_eq!(restored.appearance.id(), "dark-contrast");
        assert_eq!(restored.tabs, tabs);
        let mut invalid = serde_json::to_value(restored).unwrap();
        invalid["appearance"] = "unknown-theme".into();
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn refuses_aliases_oversize_and_invalid_references_without_replacing_preferences() {
        let dir = Temp::new();
        let path = dir.0.join("workspace.json");
        let target = dir.0.join("target");
        std::fs::write(&target, b"retained").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(load(&path).is_err());
        assert!(save(&path, &config()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(save(&path, &config()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"retained");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(load(&path).is_err());
        let mut invalid = config();
        invalid.tabs[0].device = "unregistered".into();
        assert!(save(&path, &invalid).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (MAX_BYTES + 1) as u64
        );
    }
}
