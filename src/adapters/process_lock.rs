use anyhow::{Context, Result};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
/// OS-released lock. Keep the file on disk to avoid locking different inodes.
pub struct ProcessLock {
    _file: File,
}
impl ProcessLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .context("Не удалось открыть блокировку бота")?;
        file.try_lock().map_err(|_| {
            anyhow::anyhow!(
                "Бот уже запущен или блокировка недоступна; второй экземпляр не запущен"
            )
        })?;
        Ok(Self { _file: file })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_one_open_instance_is_allowed_and_exit_releases_lock() {
        let path = std::env::temp_dir().join(format!("avito-process-lock-{}", std::process::id()));
        let first = ProcessLock::acquire(&path).unwrap();
        assert!(ProcessLock::acquire(&path).is_err());
        drop(first);
        let second = ProcessLock::acquire(&path).unwrap();
        drop(second);
        std::fs::remove_file(path).unwrap();
    }
}
