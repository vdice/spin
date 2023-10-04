use anyhow::{Context, Result};
use async_compression::tokio::write::GzipEncoder;
use std::path::PathBuf;

/// Create a compressed archive of source, returning its path in working_dir
pub async fn compressed_archive(source: &PathBuf, working_dir: &PathBuf) -> Result<PathBuf> {
    // Create tar archive file
    let tar_gz_path = working_dir
        .join(source.file_name().unwrap())
        .with_extension("tar.gz");
    let tar_gz = tokio::fs::File::create(tar_gz_path.as_path())
        .await
        .context(format!(
            "Unable to create tar archive for source {:?}",
            source.as_path()
        ))?;

    // Create encoder
    // TODO: use zstd? May be more performant
    let tar_gz_enc = GzipEncoder::new(tar_gz);

    // Build tar archive
    let mut tar_builder = async_tar::Builder::new(
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(tar_gz_enc),
    );
    tar_builder
        .append_dir_all(".", source.as_path())
        .await
        .context(format!(
            "Unable to create tar archive for source {:?}",
            source.as_path()
        ))?;
    // Finish writing the archive
    tar_builder.finish().await?;
    // Shutdown the encoder
    use tokio::io::AsyncWriteExt;
    tar_builder
        .into_inner()
        .await?
        .into_inner()
        .shutdown()
        .await?;
    Ok(tar_gz_path)
}
