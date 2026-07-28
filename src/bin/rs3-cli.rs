use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rs3::client::{ListEntry, S3Client, S3Path};
use tokio::io::{AsyncWriteExt, stdout};

#[derive(Debug, Parser)]
#[command(name = "rs3-cli", version, about = "A small mc-style S3 client")]
struct Cli {
    /// S3-compatible server endpoint
    #[arg(
        long,
        global = true,
        env = "RS3_ENDPOINT",
        default_value = "http://localhost:9000"
    )]
    endpoint: String,

    /// AWS access key ID
    #[arg(
        long,
        global = true,
        env = "RS3_ACCESS_KEY",
        default_value = "minioadmin"
    )]
    access_key: String,

    /// AWS secret access key
    #[arg(
        long,
        global = true,
        env = "RS3_SECRET_KEY",
        default_value = "minioadmin"
    )]
    secret_key: String,

    /// AWS signing region
    #[arg(long, global = true, env = "RS3_REGION", default_value = "us-east-1")]
    region: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a bucket
    Mb {
        /// Succeed when the bucket already exists, otherwise create it
        #[arg(long)]
        ensure: bool,

        /// Bucket name or s3://bucket
        bucket: String,
    },

    /// Remove an empty bucket
    Rb {
        /// Bucket name or s3://bucket
        bucket: String,
    },

    /// List buckets or objects under a prefix
    Ls {
        /// Omit to list buckets, or use s3://bucket/prefix
        target: Option<String>,

        /// Recursively list all objects below the prefix
        #[arg(short, long)]
        recursive: bool,
    },

    /// Copy one object between the local filesystem and S3
    Cp {
        /// Local path or s3://bucket/key
        source: String,

        /// Local path or s3://bucket/key
        destination: String,
    },

    /// Write an object's content to stdout
    Cat {
        /// s3://bucket/key
        target: String,
    },

    /// Show object metadata
    Stat {
        /// s3://bucket/key
        target: String,
    },

    /// Remove an object
    Rm {
        /// s3://bucket/key
        target: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = S3Client::new(&cli.endpoint, cli.access_key, cli.secret_key, cli.region)?;

    match cli.command {
        Command::Mb { bucket, ensure } => {
            let bucket = S3Path::bucket(&bucket)?;
            if ensure && client.bucket_exists(&bucket).await? {
                println!("Exists s3://{bucket}");
                return Ok(());
            }
            client.make_bucket(&bucket).await?;
            println!("Created s3://{bucket}");
        }
        Command::Rb { bucket } => {
            let bucket = S3Path::bucket(&bucket)?;
            client.remove_bucket(&bucket).await?;
            println!("Removed s3://{bucket}");
        }
        Command::Ls { target, recursive } => {
            list(&client, target.as_deref(), recursive).await?;
        }
        Command::Cp {
            source,
            destination,
        } => {
            copy(&client, &source, &destination).await?;
        }
        Command::Cat { target } => {
            let target = S3Path::parse(&target)?;
            let body = client.get_object(&target).await?;
            let mut output = stdout();
            output.write_all(&body).await?;
            output.flush().await?;
        }
        Command::Stat { target } => {
            let target = S3Path::parse(&target)?;
            let metadata = client.stat_object(&target).await?;
            println!("Name: {target}");
            println!("Size: {}", metadata.size);
            if let Some(etag) = metadata.etag {
                println!("ETag: {etag}");
            }
            if let Some(modified) = metadata.modified {
                println!("Last-Modified: {modified}");
            }
        }
        Command::Rm { target } => {
            let target = S3Path::parse(&target)?;
            client.remove_object(&target).await?;
            println!("Removed {target}");
        }
    }

    Ok(())
}

async fn list(client: &S3Client, target: Option<&str>, recursive: bool) -> Result<()> {
    let Some(target) = target else {
        for bucket in client.list_buckets().await? {
            println!("{:<20} s3://{}", bucket.creation_date, bucket.name);
        }
        return Ok(());
    };

    let target = S3Path::parse(target)?;
    for entry in client
        .list_objects(&target.bucket, &target.key, recursive)
        .await?
    {
        match entry {
            ListEntry::Prefix(prefix) => println!(
                "{:<20} {:>10} s3://{}/{}",
                "", "[DIR]", target.bucket, prefix
            ),
            ListEntry::Object {
                key,
                size,
                modified,
            } => println!(
                "{modified:<20} {:>10} s3://{}/{}",
                human_size(size),
                target.bucket,
                key
            ),
        }
    }
    Ok(())
}

async fn copy(client: &S3Client, source: &str, destination: &str) -> Result<()> {
    match (
        source.starts_with("s3://"),
        destination.starts_with("s3://"),
    ) {
        (false, true) => upload(client, Path::new(source), destination).await,
        (true, false) => download(client, source, Path::new(destination)).await,
        (true, true) => bail!("server-side S3-to-S3 copy is not supported"),
        (false, false) => bail!("one cp argument must be an s3:// path"),
    }
}

async fn upload(client: &S3Client, source: &Path, destination: &str) -> Result<()> {
    let metadata = tokio::fs::metadata(source)
        .await
        .with_context(|| format!("cannot read {}", source.display()))?;
    if !metadata.is_file() {
        bail!("recursive directory upload is not supported");
    }
    let mut destination = S3Path::parse(destination)?;
    if destination.key.is_empty() || destination.key.ends_with('/') {
        let file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("source path has no UTF-8 file name"))?;
        destination.key.push_str(file_name);
    }
    let body = tokio::fs::read(source).await?;
    client.put_object(&destination, body).await?;
    println!("Uploaded {} -> {destination}", source.display());
    Ok(())
}

async fn download(client: &S3Client, source: &str, destination: &Path) -> Result<()> {
    let source = S3Path::parse(source)?;
    let mut destination = PathBuf::from(destination);
    if tokio::fs::metadata(&destination)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        let file_name = source
            .key
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("remote path has no file name"))?;
        destination.push(file_name);
    }
    if let Some(parent) = destination.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let body = client.get_object(&source).await?;
    tokio::fs::write(&destination, body).await?;
    println!("Downloaded {source} -> {}", destination.display());
    Ok(())
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, human_size};

    #[test]
    fn formats_human_readable_sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KiB");
    }

    #[test]
    fn parses_ensure_make_bucket_option() {
        let cli = Cli::parse_from(["rs3-cli", "mb", "--ensure", "s3://bucket"]);
        match cli.command {
            Command::Mb { bucket, ensure } => {
                assert_eq!(bucket, "s3://bucket");
                assert!(ensure);
            }
            _ => panic!("expected mb command"),
        }
    }
}
