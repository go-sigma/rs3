use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use walkdir::WalkDir;

const FOLDER_MARKER: &str = ".folder_marker";

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug)]
pub struct CompletedUpload {
    pub etag: String,
}

impl Storage {
    pub async fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    pub fn bucket_path(&self, bucket: &str) -> PathBuf {
        self.root.join(bucket)
    }

    pub fn object_path(&self, bucket: &str, key: &str) -> PathBuf {
        let bucket = self.bucket_path(bucket);
        if key.ends_with('/') {
            bucket.join(format!("{key}{FOLDER_MARKER}"))
        } else {
            bucket.join(key)
        }
    }

    pub async fn create_bucket(&self, bucket: &str) -> io::Result<()> {
        match fs::create_dir(self.bucket_path(bucket)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub async fn bucket_exists(&self, bucket: &str) -> bool {
        fs::metadata(self.bucket_path(bucket))
            .await
            .is_ok_and(|metadata| metadata.is_dir())
    }

    pub async fn delete_bucket(&self, bucket: &str) -> io::Result<()> {
        fs::remove_dir(self.bucket_path(bucket)).await
    }

    pub async fn list_buckets(&self) -> io::Result<Vec<ObjectInfo>> {
        let mut entries = fs::read_dir(&self.root).await?;
        let mut buckets = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let metadata = entry.metadata().await?;
            if metadata.is_dir() {
                buckets.push(ObjectInfo {
                    key: name,
                    size: 0,
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
        buckets.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(buckets)
    }

    pub async fn put_object(&self, bucket: &str, key: &str, body: &[u8]) -> io::Result<()> {
        let path = self.object_path(bucket, key);
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "object has no parent"))?;
        fs::create_dir_all(parent).await?;

        let temporary = parent.join(format!(".rs3-write-{}", Uuid::new_v4()));
        let mut file = fs::File::create(&temporary).await?;
        if let Err(error) = async {
            file.write_all(body).await?;
            file.flush().await?;
            drop(file);
            fs::rename(&temporary, &path).await
        }
        .await
        {
            let _ = fs::remove_file(&temporary).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn read_object(&self, bucket: &str, key: &str) -> io::Result<Vec<u8>> {
        fs::read(self.object_path(bucket, key)).await
    }

    pub async fn object_metadata(&self, bucket: &str, key: &str) -> io::Result<std::fs::Metadata> {
        fs::metadata(self.object_path(bucket, key)).await
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> io::Result<()> {
        let path = self.object_path(bucket, key);
        match fs::remove_file(&path).await {
            Ok(()) => self.remove_empty_parents(bucket, path.parent()).await,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn remove_empty_parents(
        &self,
        bucket: &str,
        mut parent: Option<&Path>,
    ) -> io::Result<()> {
        let bucket_path = self.bucket_path(bucket);
        while let Some(path) = parent {
            if path == bucket_path || !path.starts_with(&bucket_path) {
                break;
            }
            match fs::remove_dir(path).await {
                Ok(()) => parent = path.parent(),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub async fn list_objects(&self, bucket: &str) -> io::Result<Vec<ObjectInfo>> {
        let bucket_path = self.bucket_path(bucket);
        tokio::task::spawn_blocking(move || collect_objects(&bucket_path))
            .await
            .map_err(io::Error::other)?
    }

    pub async fn create_upload(&self, bucket: &str, key: &str) -> io::Result<String> {
        let upload_id = Uuid::new_v4().simple().to_string();
        let upload_dir = self.upload_path(&upload_id);
        fs::create_dir_all(&upload_dir).await?;
        fs::write(upload_dir.join(".meta"), format!("{bucket}\n{key}")).await?;
        Ok(upload_id)
    }

    pub async fn put_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        body: &[u8],
    ) -> io::Result<String> {
        self.verify_upload(upload_id, bucket, key).await?;
        fs::write(
            self.upload_path(upload_id).join(part_number.to_string()),
            body,
        )
        .await?;
        Ok(md5_etag(body))
    }

    pub async fn complete_upload(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[u32],
    ) -> io::Result<CompletedUpload> {
        self.verify_upload(upload_id, bucket, key).await?;
        let upload_dir = self.upload_path(upload_id);
        let mut parts = if requested_parts.is_empty() {
            list_part_numbers(&upload_dir).await?
        } else {
            requested_parts.to_vec()
        };
        parts.sort_unstable();
        parts.dedup();

        let final_path = self.object_path(bucket, key);
        let parent = final_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "object has no parent"))?;
        fs::create_dir_all(parent).await?;
        let temporary = parent.join(format!(".rs3-multipart-{}", Uuid::new_v4()));
        let mut output = fs::File::create(&temporary).await?;
        let mut multipart_hash = Md5::new();

        let result = async {
            for part_number in &parts {
                let part = fs::read(upload_dir.join(part_number.to_string())).await?;
                output.write_all(&part).await?;
                multipart_hash.update(Md5::digest(&part));
            }
            output.flush().await?;
            drop(output);
            fs::rename(&temporary, &final_path).await?;
            fs::remove_dir_all(&upload_dir).await?;
            Ok::<(), io::Error>(())
        }
        .await;

        if let Err(error) = result {
            let _ = fs::remove_file(&temporary).await;
            return Err(error);
        }

        Ok(CompletedUpload {
            etag: format!("{}-{}", hex::encode(multipart_hash.finalize()), parts.len()),
        })
    }

    pub async fn abort_upload(&self, upload_id: &str, bucket: &str, key: &str) -> io::Result<()> {
        self.verify_upload(upload_id, bucket, key).await?;
        fs::remove_dir_all(self.upload_path(upload_id)).await
    }

    async fn verify_upload(&self, upload_id: &str, bucket: &str, key: &str) -> io::Result<()> {
        let metadata = fs::read_to_string(self.upload_path(upload_id).join(".meta")).await?;
        let (stored_bucket, stored_key) = metadata.split_once('\n').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid multipart metadata")
        })?;
        if stored_bucket != bucket || stored_key != key {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "multipart upload does not match object",
            ));
        }
        Ok(())
    }

    fn upload_path(&self, upload_id: &str) -> PathBuf {
        self.root.join(".uploads").join(upload_id)
    }
}

pub fn md5_etag(body: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Md5::digest(body)))
}

fn collect_objects(bucket_path: &Path) -> io::Result<Vec<ObjectInfo>> {
    let mut objects = Vec::new();
    for entry in WalkDir::new(bucket_path).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(bucket_path)
            .map_err(io::Error::other)?;
        if has_unsafe_component(relative) {
            continue;
        }

        let mut key = relative.to_string_lossy().replace('\\', "/");
        if key.ends_with(FOLDER_MARKER) {
            key.truncate(key.len() - FOLDER_MARKER.len());
        } else if relative
            .components()
            .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }

        let metadata = entry.metadata().map_err(io::Error::other)?;
        objects.push(ObjectInfo {
            key,
            size: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    objects.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(objects)
}

fn has_unsafe_component(path: &Path) -> bool {
    path.components().any(|component| {
        !matches!(component, Component::Normal(_))
            || component.as_os_str().to_string_lossy() == ".uploads"
    })
}

async fn list_part_numbers(upload_dir: &Path) -> io::Result<Vec<u32>> {
    let mut entries = fs::read_dir(upload_dir).await?;
    let mut parts = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if let Ok(part_number) = entry.file_name().to_string_lossy().parse() {
            parts.push(part_number);
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::md5_etag;

    #[test]
    fn etag_is_quoted_md5() {
        assert_eq!(md5_etag(b"hello"), "\"5d41402abc4b2a76b9719d911017c592\"");
    }
}
