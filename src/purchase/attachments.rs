use super::store::{Store, now};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};
pub enum Download {
    Bytes { data: Vec<u8>, mime: String },
    Unavailable,
}
pub trait AttachmentSource {
    fn fetch(&mut self, url: &str) -> Result<Download>;
}
pub fn allowed_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    let host = url.host_str().context("Attachment host missing")?;
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none(),
        "Attachment requires plain HTTPS authority"
    );
    ensure!(
        ["avito.ru", "avito.st", "avito.com"]
            .iter()
            .any(|base| host == *base || host.ends_with(&format!(".{base}"))),
        "Attachment host outside Avito is not downloaded"
    );
    Ok(url)
}
pub struct HttpAttachments {
    client: reqwest::blocking::Client,
}
impl HttpAttachments {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }
}
impl AttachmentSource for HttpAttachments {
    fn fetch(&mut self, raw: &str) -> Result<Download> {
        let mut url = allowed_url(raw)?;
        for _ in 0..5 {
            crate::adapters::network::require_direct()?;
            let response =
                self.client.get(url.clone()).send().map_err(|e| {
                    anyhow::anyhow!("Attachment request failed: {}", e.without_url())
                })?;
            if response.status().is_redirection() {
                let target = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("Attachment redirect has no location")?
                    .to_str()?;
                url = allowed_url(url.join(target)?.as_str())?;
                continue;
            }
            if [401, 403, 404, 410].contains(&response.status().as_u16()) {
                return Ok(Download::Unavailable);
            }
            ensure!(
                response.status().is_success(),
                "Attachment HTTP {}; queue will pause",
                response.status()
            );
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            ensure!(
                !mime.starts_with("text/html"),
                "Unexpected HTML instead of attachment; queue paused"
            );
            const LIMIT: u64 = 20 * 1024 * 1024;
            ensure!(
                response.content_length().unwrap_or(0) <= LIMIT,
                "Attachment exceeds 20 MB limit"
            );
            let mut data = vec![];
            response.take(LIMIT + 1).read_to_end(&mut data)?;
            ensure!(data.len() as u64 <= LIMIT, "Attachment exceeds 20 MB limit");
            return Ok(Download::Bytes { data, mime });
        }
        anyhow::bail!("Attachment redirect limit reached")
    }
}
pub fn save(
    store: &Store,
    chat: &str,
    message: &str,
    reference: &str,
    directory: &Path,
    source: &mut impl AttachmentSource,
) -> Result<()> {
    match source.fetch(reference)? {
        Download::Unavailable => {
            store.db.execute("UPDATE archived_attachments SET status='unavailable' WHERE chat_id=?1 AND message_id=?2 AND reference=?3",params![chat,message,reference])?;
        }
        Download::Bytes { data, mime } => {
            std::fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
            }
            let digest = format!("{:x}", Sha256::digest(&data));
            let path = directory.join(format!("{digest}.bin"));
            if !path.exists() {
                let temporary = directory.join(format!(".{digest}-{}.tmp", std::process::id()));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temporary)?;
                file.write_all(&data)?;
                file.sync_all()?;
                std::fs::rename(temporary, &path)?;
            }
            ensure!(
                format!("{:x}", Sha256::digest(std::fs::read(&path)?)) == digest,
                "Attachment checksum mismatch"
            );
            store.db.execute("INSERT INTO attachment_blobs(chat_id,message_id,reference,digest,path,mime,size,downloaded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(chat_id,message_id,reference) DO UPDATE SET digest=excluded.digest,path=excluded.path,mime=excluded.mime,size=excluded.size,downloaded_at=excluded.downloaded_at",params![chat,message,reference,digest,path.to_str().context("Invalid attachment path")?,mime,data.len(),now()])?;
            store.db.execute("UPDATE archived_attachments SET status='saved',local_path=?4 WHERE chat_id=?1 AND message_id=?2 AND reference=?3",params![chat,message,reference,path.to_str()])?;
        }
    }
    Ok(())
}
pub fn queue_pending(store: &Store) -> Result<usize> {
    let rows = {
        let mut s=store.db.prepare("SELECT chat_id,message_id,reference FROM archived_attachments WHERE status='pending' LIMIT 50")?;
        s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut count = 0;
    for (chat, message, reference) in rows {
        if allowed_url(&reference).is_ok() {
            store.enqueue(
                super::jobs::Job::Attachment {
                    chat,
                    message,
                    reference,
                },
                1,
            )?;
            count += 1;
        } else {
            store.db.execute("UPDATE archived_attachments SET status='unsupported' WHERE chat_id=?1 AND message_id=?2 AND reference=?3",params![chat,message,reference])?;
        }
    }
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachment_redirect_targets_must_stay_on_avito() {
        for raw in [
            "http://avito.ru/file",
            "https://avito.ru.evil.test/a",
            "https://evil.test/avito.ru",
            "https://user@avito.ru/file",
            "https://127.0.0.1/file",
        ] {
            assert!(allowed_url(raw).is_err(), "{raw}")
        }
        assert!(allowed_url("https://01.img.avito.st/image/a").is_ok());
    }
    struct Fake;
    impl AttachmentSource for Fake {
        fn fetch(&mut self, _: &str) -> Result<Download> {
            Ok(Download::Bytes {
                data: b"fixture".to_vec(),
                mime: "image/png".into(),
            })
        }
    }
    #[test]
    fn saved_bytes_are_checksummed_and_reference_is_persisted() {
        let dir = std::env::temp_dir().join(format!("avito-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("test.sqlite")).unwrap();
        store.db.execute("INSERT INTO archived_attachments(chat_id,message_id,reference,payload) VALUES('chat','message','https://avito.st/a','{}')",[]).unwrap();
        save(
            &store,
            "chat",
            "message",
            "https://avito.st/a",
            &dir.join("files"),
            &mut Fake,
        )
        .unwrap();
        let status: String = store
            .db
            .query_row("SELECT status FROM archived_attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "saved");
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM attachment_blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
