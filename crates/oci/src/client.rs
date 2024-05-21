//! Spin's client for distributing applications via OCI registries

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use docker_credential::DockerCredential;
use futures_util::future;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use itertools::Itertools;
use oci_distribution::{
    client::ImageLayer, config::ConfigFile, manifest::OciImageManifest, secrets::RegistryAuth,
    token_cache::RegistryTokenType, Reference, RegistryOperation,
};
use reqwest::Url;
use spin_common::sha256;
use spin_common::ui::quoted_path;
use spin_common::url::parse_file_url;
use spin_loader::cache::Cache;
use spin_loader::FilesMountStrategy;
use spin_locked_app::locked::{ContentPath, ContentRef, LockedApp};
use tokio::fs;
use walkdir::WalkDir;

use crate::auth::AuthConfig;

// TODO: the media types for application, data and archive layer are not final
/// Media type for a layer representing a locked Spin application configuration
pub const SPIN_APPLICATION_MEDIA_TYPE: &str = "application/vnd.fermyon.spin.application.v1+config";
/// Media type for a layer representing a generic data file used by a Spin application
pub const DATA_MEDIATYPE: &str = "application/vnd.wasm.content.layer.v1+data";
/// Media type for a layer representing a compressed archive of one or more files used by a Spin application
pub const ARCHIVE_MEDIATYPE: &str = "application/vnd.wasm.content.bundle.v1.tar+gzip";
// Note: this will be updated with a canonical value once defined upstream
const WASM_LAYER_MEDIA_TYPE: &str = "application/vnd.wasm.content.layer.v1+wasm";

const CONFIG_FILE: &str = "config.json";
const LATEST_TAG: &str = "latest";
const MANIFEST_FILE: &str = "manifest.json";

const MAX_PARALLEL_PULL: usize = 16;
/// Maximum layer count allowed per app, set in accordance to the lowest
/// known maximum per image in well-known OCI registry implementations.
/// (500 appears to be the limit for Elastic Container Registry)
const MAX_LAYER_COUNT: usize = 500;

/// Default maximum content size for inlining directly into config,
/// rather than pushing as a separate layer
const DEFAULT_CONTENT_REF_INLINE_MAX_SIZE: usize = 128;

/// Default token expiration when pushing/pulling an image to/from a registry.
/// This value is used by the underyling OCI client when the token expiration
/// is unspecified on a claim.
/// This essentially equates to a timeout for push/pull.
const DEFAULT_TOKEN_EXPIRATION_SECS: usize = 300;

/// Mode of assembly of a Spin application into an OCI image
enum AssemblyMode {
    /// Assemble the application as one layer per component and one layer for
    /// every static asset included with a given component
    Simple,
    /// Assemble the application as one layer per component and one compressed
    /// archive layer containing all static assets included with a given component
    Archive,
}

/// Client for interacting with an OCI registry for Spin applications.
pub struct Client {
    /// Global cache for the metadata, Wasm modules, and static assets pulled from OCI registries.
    pub cache: Cache,
    /// Underlying OCI client.
    oci: oci_distribution::Client,
    /// Client options
    pub opts: ClientOpts,
}

#[derive(Clone)]
/// Options for configuring a Client
pub struct ClientOpts {
    /// Inline content into ContentRef iff < this size.
    pub content_ref_inline_max_size: usize,
}

impl Client {
    /// Create a new instance of an OCI client for distributing Spin applications.
    pub async fn new(insecure: bool, cache_root: Option<PathBuf>) -> Result<Self> {
        let client = oci_distribution::Client::new(Self::build_config(insecure));
        let cache = Cache::new(cache_root).await?;
        let opts = ClientOpts {
            content_ref_inline_max_size: DEFAULT_CONTENT_REF_INLINE_MAX_SIZE,
        };

        Ok(Self {
            oci: client,
            cache,
            opts,
        })
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    pub async fn push(
        &mut self,
        manifest_path: &Path,
        reference: impl AsRef<str>,
        annotations: Option<BTreeMap<String, String>>,
    ) -> Result<Option<String>> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .with_context(|| format!("cannot parse reference {}", reference.as_ref()))?;
        let auth = Self::auth(&reference).await?;
        let working_dir = tempfile::tempdir()?;

        // Create a locked application from the application manifest.
        // TODO: We don't need an extra copy here for each asset to prepare the application.
        // We should be able to use assets::collect instead when constructing the locked app.
        let locked = spin_loader::from_file(
            manifest_path,
            FilesMountStrategy::Copy(working_dir.path().into()),
            None,
        )
        .await?;

        self.push_locked_core(locked, auth, reference, annotations)
            .await
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    pub async fn push_locked(
        &mut self,
        locked: LockedApp,
        reference: impl AsRef<str>,
        annotations: Option<BTreeMap<String, String>>,
    ) -> Result<Option<String>> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .with_context(|| format!("cannot parse reference {}", reference.as_ref()))?;
        let auth = Self::auth(&reference).await?;

        self.push_locked_core(locked, auth, reference, annotations)
            .await
    }

    /// Push a Spin application to an OCI registry and return the digest (or None
    /// if the digest cannot be determined).
    async fn push_locked_core(
        &mut self,
        locked: LockedApp,
        auth: RegistryAuth,
        reference: Reference,
        annotations: Option<BTreeMap<String, String>>,
    ) -> Result<Option<String>> {
        let mut locked_app = locked.clone();
        let mut layers = self
            .assemble_layers(&mut locked_app, AssemblyMode::Simple)
            .await
            .context("could not assemble layers for locked application")?;

        // If layer count exceeds MAX_LAYER_COUNT-1, assemble archive layers instead.
        // (We'll be adding one more layer to represent the locked application config.)
        if layers.len() > MAX_LAYER_COUNT - 1 {
            locked_app = locked.clone();
            layers = self
                .assemble_layers(&mut locked_app, AssemblyMode::Archive)
                .await
                .context("could not assemble archive layers for locked application")?;
        }

        // Push layer for locked spin application config
        let locked_config_layer = ImageLayer::new(
            serde_json::to_vec(&locked_app).context("could not serialize locked config")?,
            SPIN_APPLICATION_MEDIA_TYPE.to_string(),
            None,
        );
        let config_layer_digest = locked_config_layer.sha256_digest().clone();
        layers.push(locked_config_layer);

        let mut labels = HashMap::new();
        labels.insert(
            "com.fermyon.spin.lockedAppDigest".to_string(),
            config_layer_digest,
        );
        let cfg = oci_distribution::config::Config {
            labels: Some(labels),
            ..Default::default()
        };

        // Construct empty/default OCI config file. Data may be parsed according to
        // the expected config structure per the image spec, so we want to ensure it conforms.
        // (See https://github.com/opencontainers/image-spec/blob/main/config.md)
        // TODO: Explore adding data applicable to the Spin app being published.
        let oci_config_file = ConfigFile {
            architecture: oci_distribution::config::Architecture::Wasm,
            os: oci_distribution::config::Os::Wasip1,
            // We need to ensure that the image config for different content is updated.
            // Without referencing the digest of the locked application in the OCI image config,
            // all Spin applications would get the same image config digest, resulting in the same
            // image ID in container runtimes.
            config: Some(cfg),
            ..Default::default()
        };
        let oci_config =
            oci_distribution::client::Config::oci_v1_from_config_file(oci_config_file, None)?;
        let manifest = OciImageManifest::build(&layers, &oci_config, annotations);

        let response = self
            .oci
            .push(&reference, &layers, oci_config, &auth, Some(manifest))
            .await
            .map(|push_response| push_response.manifest_url)
            .context("cannot push Spin application")?;

        tracing::info!("Pushed {:?}", response);

        let digest = digest_from_url(&response);
        Ok(digest)
    }

    /// Assemble ImageLayers for a locked application using the provided
    /// AssemblyMode and return the resulting Vec<ImageLayer>.
    async fn assemble_layers(
        &mut self,
        locked: &mut LockedApp,
        assembly_mode: AssemblyMode,
    ) -> Result<Vec<ImageLayer>> {
        let mut layers = Vec::new();
        let mut components = Vec::new();
        for mut c in locked.clone().components {
            // Add the wasm module for the component as layers.
            let source = c
                .clone()
                .source
                .content
                .source
                .context("component loaded from disk should contain a file source")?;

            let source = parse_file_url(source.as_str())?;
            let layer = Self::wasm_layer(&source).await?;

            // Update the module source with the content ref of the layer.
            c.source.content = self.content_ref_for_layer(&layer);

            layers.push(layer);

            let mut files = Vec::new();
            for f in c.files {
                let source = f
                    .content
                    .source
                    .context("file mount loaded from disk should contain a file source")?;
                let source = parse_file_url(source.as_str())?;

                match assembly_mode {
                    AssemblyMode::Archive => self
                        .push_archive_layer(&source, &mut files, &mut layers)
                        .await
                        .context(format!(
                            "cannot push archive layer for source {}",
                            quoted_path(&source)
                        ))?,
                    AssemblyMode::Simple => self
                        .push_file_layers(&source, &mut files, &mut layers)
                        .await
                        .context(format!(
                            "cannot push file layers for source {}",
                            quoted_path(&source)
                        ))?,
                }
            }
            c.files = files;
            components.push(c);
        }
        locked.components = components;
        locked.metadata.remove("origin");

        // Deduplicate layers
        layers = layers.into_iter().unique().collect();

        Ok(layers)
    }

    /// Archive all of the files recursively under the source directory
    /// and push as a compressed archive layer
    async fn push_archive_layer(
        &mut self,
        source: &PathBuf,
        files: &mut Vec<ContentPath>,
        layers: &mut Vec<ImageLayer>,
    ) -> Result<()> {
        // Add all archived file entries to the locked app manifest
        for entry in WalkDir::new(source) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            // Can unwrap because we got to 'entry' from walking 'source'
            let rel_path = entry.path().strip_prefix(source).unwrap();
            tracing::trace!("Adding asset {rel_path:?} to component files list");
            // Add content/path to the locked component files list
            let layer = Self::data_layer(entry.path(), DATA_MEDIATYPE.to_string()).await?;
            let content = self.content_ref_for_layer(&layer);
            files.push(ContentPath {
                content,
                path: rel_path.into(),
            });
        }

        // Only add the archive layer to the OCI manifest
        tracing::trace!("Adding archive layer for all files in source {:?}", &source);
        let working_dir = tempfile::tempdir()?;
        let archive_path = crate::utils::archive(source, &working_dir.into_path())
            .await
            .context(format!(
                "Unable to create compressed archive for source {:?}",
                source
            ))?;
        let layer = Self::data_layer(archive_path.as_path(), ARCHIVE_MEDIATYPE.to_string()).await?;
        layers.push(layer);
        Ok(())
    }

    /// Recursively traverse the source directory and add layers for each file.
    async fn push_file_layers(
        &mut self,
        source: &PathBuf,
        files: &mut Vec<ContentPath>,
        layers: &mut Vec<ImageLayer>,
    ) -> Result<()> {
        // Traverse each mount directory, add all static assets as layers, then update the
        // locked application file with the file digest.
        tracing::trace!("Adding new layer per file under source {:?}", source);
        for entry in WalkDir::new(source) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            // Can unwrap because we got to 'entry' from walking 'source'
            let rel_path = entry.path().strip_prefix(source).unwrap();
            tracing::trace!("Adding new layer for asset {rel_path:?}");
            // Construct and push layer, adding its digest to the locked component files Vec
            let layer = Self::data_layer(entry.path(), DATA_MEDIATYPE.to_string()).await?;
            let content = self.content_ref_for_layer(&layer);
            let content_inline = content.inline.is_some();
            files.push(ContentPath {
                content,
                path: rel_path.into(),
            });
            // As a workaround for OCI implementations that don't support very small blobs,
            // don't push very small content that has been inlined into the manifest:
            // https://github.com/distribution/distribution/discussions/4029
            let skip_layer = content_inline;
            if !skip_layer {
                layers.push(layer);
            }
        }
        Ok(())
    }

    /// Pull a Spin application from an OCI registry.
    pub async fn pull(&mut self, reference: &str) -> Result<()> {
        let reference: Reference = reference.parse().context("cannot parse reference")?;
        let auth = Self::auth(&reference).await?;

        // Pull the manifest from the registry.
        let (manifest, digest) = self.oci.pull_image_manifest(&reference, &auth).await?;

        let manifest_json = serde_json::to_string(&manifest)?;
        tracing::debug!("Pulled manifest: {}", manifest_json);

        // Write the manifest in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/manifest.json`
        let m = self.manifest_path(&reference.to_string()).await?;
        fs::write(&m, &manifest_json).await?;

        // Older published Spin apps feature the locked app config *as* the OCI manifest config layer,
        // while newer versions publish the locked app config as a generic layer alongside others.
        // Assume that these bytes may represent the locked app config and write it as such.
        let mut cfg_bytes = Vec::new();
        self.oci
            .pull_blob(&reference, &manifest.config, &mut cfg_bytes)
            .await?;
        self.write_locked_app_config(&reference.to_string(), &cfg_bytes)
            .await
            .context("unable to write locked app config to cache")?;

        // If a layer is a Wasm module, write it in the Wasm directory.
        // Otherwise, write it in the data directory (after unpacking if archive layer)
        stream::iter(manifest.layers)
            .map(|layer| {
                let this = &self;
                let reference = reference.clone();
                async move {
                    // Skip pulling if the digest already exists in the wasm or data directories.
                    if this.cache.wasm_file(&layer.digest).is_ok()
                        || this.cache.data_file(&layer.digest).is_ok()
                    {
                        tracing::debug!("Layer {} already exists in cache", &layer.digest);
                        return anyhow::Ok(());
                    }

                    tracing::debug!("Pulling layer {}", &layer.digest);
                    let mut bytes = Vec::with_capacity(layer.size.try_into()?);
                    this.oci.pull_blob(&reference, &layer, &mut bytes).await?;
                    match layer.media_type.as_str() {
                        SPIN_APPLICATION_MEDIA_TYPE => {
                            this.write_locked_app_config(&reference.to_string(), &bytes)
                                .await
                                .with_context(|| "unable to write locked app config to cache")?;
                        }
                        WASM_LAYER_MEDIA_TYPE => {
                            this.cache.write_wasm(&bytes, &layer.digest).await?;
                        }
                        ARCHIVE_MEDIATYPE => {
                            unpack_archive_layer(&this.cache, &bytes, &layer.digest).await?;
                        }
                        _ => {
                            this.cache.write_data(&bytes, &layer.digest).await?;
                        }
                    }
                    Ok(())
                }
            })
            .buffer_unordered(MAX_PARALLEL_PULL)
            .try_for_each(future::ok)
            .await?;
        tracing::info!("Pulled {}@{}", reference, digest);

        Ok(())
    }

    /// Get the file path to an OCI manifest given a reference.
    /// If the directory for the manifest does not exist, this will create it.
    async fn manifest_path(&self, reference: impl AsRef<str>) -> Result<PathBuf> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .context("cannot parse OCI reference")?;
        let p = self
            .cache
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or(LATEST_TAG));

        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .context("cannot find directory for OCI manifest")?;
        }

        Ok(p.join(MANIFEST_FILE))
    }

    /// Get the file path to the OCI configuration object given a reference.
    pub async fn lockfile_path(&self, reference: impl AsRef<str>) -> Result<PathBuf> {
        let reference: Reference = reference
            .as_ref()
            .parse()
            .context("cannot parse reference")?;
        let p = self
            .cache
            .manifests_dir()
            .join(reference.registry())
            .join(reference.repository())
            .join(reference.tag().unwrap_or(LATEST_TAG));

        if !p.is_dir() {
            fs::create_dir_all(&p)
                .await
                .context("cannot find configuration object for reference")?;
        }

        Ok(p.join(CONFIG_FILE))
    }

    /// Write the config object in `<cache_root>/registry/oci/manifests/repository:<tag_or_latest>/config.json`
    async fn write_locked_app_config(
        &self,
        reference: impl AsRef<str>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<()> {
        let cfg = std::str::from_utf8(bytes.as_ref())?;
        tracing::debug!("Pulled config: {}", cfg);

        let c = self.lockfile_path(reference).await?;
        fs::write(&c, &cfg).await.map_err(anyhow::Error::from)
    }

    /// Create a new wasm layer based on a file.
    async fn wasm_layer(file: &Path) -> Result<ImageLayer> {
        tracing::log::trace!("Reading wasm module from {:?}", file);
        Ok(ImageLayer::new(
            fs::read(file).await.context("cannot read wasm module")?,
            WASM_LAYER_MEDIA_TYPE.to_string(),
            None,
        ))
    }

    /// Create a new data layer based on a file.
    async fn data_layer(file: &Path, media_type: String) -> Result<ImageLayer> {
        tracing::log::trace!("Reading data file from {:?}", file);
        Ok(ImageLayer::new(fs::read(&file).await?, media_type, None))
    }

    fn content_ref_for_layer(&self, layer: &ImageLayer) -> ContentRef {
        ContentRef {
            // Inline small content as an optimization and to work around issues
            // with OCI implementations that don't support very small blobs.
            inline: (layer.data.len() <= self.opts.content_ref_inline_max_size)
                .then(|| layer.data.to_vec()),
            digest: Some(layer.sha256_digest()),
            ..Default::default()
        }
    }

    /// Save a credential set containing the registry username and password.
    pub async fn login(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        // We want to allow a user to login to both https://ghcr.io and ghcr.io.
        let server = server.as_ref();
        let server = match server.parse::<Url>() {
            Ok(url) => url.host_str().unwrap_or(server).to_string(),
            Err(_) => server.to_string(),
        };

        // First, validate the credentials. If a user accidentally enters a wrong credential set, this
        // can catch the issue early rather than getting an error at the first operation that needs
        // to use the credentials (first time they do a push/pull/up).
        Self::validate_credentials(&server, &username, &password).await?;

        // Save an encoded representation of the credential set in the local configuration file.
        let mut auth = AuthConfig::load_default().await?;
        auth.insert(server, username, password)?;
        auth.save_default().await
    }

    /// Insert a token in the OCI client token cache.
    pub async fn insert_token(
        &mut self,
        reference: &Reference,
        op: RegistryOperation,
        token: RegistryTokenType,
    ) {
        self.oci.tokens.insert(reference, op, token).await;
    }

    /// Validate the credentials by attempting to send an authenticated request to the registry.
    async fn validate_credentials(
        server: impl AsRef<str>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<()> {
        let client = dkregistry::v2::Client::configure()
            .registry(server.as_ref())
            .insecure_registry(false)
            .username(Some(username.as_ref().into()))
            .password(Some(password.as_ref().into()))
            .build()
            .context("cannot create client to send authentication request to the registry")?;

        match client
            // We don't need to configure any scopes, we are only testing that the credentials are
            // valid for the intended registry.
            .authenticate(&[""])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => bail!(format!(
                "cannot authenticate as {} to registry {}: {}",
                username.as_ref(),
                server.as_ref(),
                e
            )),
        }
    }

    /// Construct the registry authentication based on the reference.
    async fn auth(reference: &Reference) -> Result<RegistryAuth> {
        let server = reference
            .resolve_registry()
            .strip_suffix('/')
            .unwrap_or_else(|| reference.resolve_registry());

        match AuthConfig::get_auth_from_default(server).await {
            Ok(c) => Ok(c),
            Err(_) => match docker_credential::get_credential(server) {
                Err(e) => {
                    tracing::trace!("Cannot retrieve credentials from Docker, attempting to use anonymous auth: {}", e);
                    Ok(RegistryAuth::Anonymous)
                }

                Ok(DockerCredential::UsernamePassword(username, password)) => {
                    tracing::trace!("Found Docker credentials");
                    Ok(RegistryAuth::Basic(username, password))
                }
                Ok(DockerCredential::IdentityToken(_)) => {
                    tracing::trace!("Cannot use contents of Docker config, identity token not supported. Using anonymous auth");
                    Ok(RegistryAuth::Anonymous)
                }
            },
        }
    }

    /// Build the OCI client configuration given the insecure option.
    fn build_config(insecure: bool) -> oci_distribution::client::ClientConfig {
        let protocol = if insecure {
            oci_distribution::client::ClientProtocol::Http
        } else {
            oci_distribution::client::ClientProtocol::Https
        };

        oci_distribution::client::ClientConfig {
            protocol,
            default_token_expiration_secs: DEFAULT_TOKEN_EXPIRATION_SECS,
            ..Default::default()
        }
    }
}

/// Unpack archive layer into self.cache
pub async fn unpack_archive_layer(
    cache: &Cache,
    bytes: impl AsRef<[u8]>,
    digest: impl AsRef<str>,
) -> Result<()> {
    // Write archive layer to cache as usual
    cache.write_data(&bytes, &digest).await?;

    // Unpack archive into a staging dir
    let path = cache
        .data_file(&digest)
        .context("unable to read archive layer from cache")?;
    let staging_dir = tempfile::tempdir()?;
    crate::utils::unarchive(path.as_ref(), staging_dir.path()).await?;

    // Traverse unpacked contents and if a file, write to cache by digest
    // (if it doesn't already exist)
    for entry in WalkDir::new(staging_dir.path()) {
        let entry = entry?;
        if entry.file_type().is_file() && !entry.file_type().is_dir() {
            let bytes = tokio::fs::read(entry.path()).await?;
            let digest = format!("sha256:{}", sha256::hex_digest_from_bytes(&bytes));
            if cache.data_file(&digest).is_ok() {
                tracing::debug!(
                    "Skipping unpacked asset {:?}; file already exists",
                    entry.path()
                );
            } else {
                tracing::debug!("Adding unpacked asset {:?} to cache", entry.path());
                cache.write_data(bytes, &digest).await?;
            }
        }
    }
    Ok(())
}

fn digest_from_url(manifest_url: &str) -> Option<String> {
    // The URL is in the form "https://host/v2/refname/manifests/sha256:..."
    let manifest_url = Url::parse(manifest_url).ok()?;
    let segments = manifest_url.path_segments()?;
    let last = segments.last()?;
    if last.contains(':') {
        Some(last.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn can_parse_digest_from_manifest_url() {
        let manifest_url = "https://ghcr.io/v2/itowlson/osf/manifests/sha256:0a867093096e0ef01ef749b12b6e7a90e4952eda107f89a676eeedce63a8361f";
        let digest = digest_from_url(manifest_url).unwrap();
        assert_eq!(
            "sha256:0a867093096e0ef01ef749b12b6e7a90e4952eda107f89a676eeedce63a8361f",
            digest
        );
    }

    #[tokio::test]
    async fn can_assemble_layers() {
        use spin_locked_app::locked::LockedComponent;
        use tokio::io::AsyncWriteExt;

        let working_dir = tempfile::tempdir().unwrap();

        // Set up component/file directory tree
        //
        // create component1 and component2 dirs
        let _ = tokio::fs::create_dir(working_dir.path().join("component1").as_path()).await;
        let _ = tokio::fs::create_dir(working_dir.path().join("component2").as_path()).await;

        // create component "wasm" files
        let mut c1 = tokio::fs::File::create(working_dir.path().join("component1.wasm"))
            .await
            .expect("should create component wasm file");
        c1.write_all(b"c1")
            .await
            .expect("should write component wasm contents");
        let mut c2 = tokio::fs::File::create(working_dir.path().join("component2.wasm"))
            .await
            .expect("should create component wasm file");
        c2.write_all(b"c2")
            .await
            .expect("should write component wasm contents");

        // component1 files
        let mut c1f1 = tokio::fs::File::create(working_dir.path().join("component1").join("bar"))
            .await
            .expect("should create component file");
        c1f1.write_all(b"bar")
            .await
            .expect("should write file contents");
        let mut c1f2 = tokio::fs::File::create(working_dir.path().join("component1").join("baz"))
            .await
            .expect("should create component file");
        c1f2.write_all(b"baz")
            .await
            .expect("should write file contents");

        // component2 files
        let mut c2f1 = tokio::fs::File::create(working_dir.path().join("component2").join("baz"))
            .await
            .expect("should create component file");
        c2f1.write_all(b"baz")
            .await
            .expect("should write file contents");

        #[derive(Clone)]
        struct TestCase {
            name: &'static str,
            opts: Option<ClientOpts>,
            locked_components: Vec<LockedComponent>,
            expected_layer_count: usize,
            expected_error: Option<&'static str>,
        }

        let tests: Vec<TestCase> = [
            TestCase {
                name: "Two component layers",
                opts: None,
                locked_components: spin_testing::from_json!([{
                    "id": "component1",
                    "source": {
                        "content_type": "application/wasm",
                        "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                        "digest": "digest",
                }},
                {
                    "id": "component2",
                    "source": {
                        "content_type": "application/wasm",
                        "source": format!("file://{}", working_dir.path().join("component2.wasm").to_str().unwrap()),
                        "digest": "digest",
                }}]),
                expected_layer_count: 2,
                expected_error: None,
            },
            TestCase {
                name: "One component layer and two file layers",
                opts: Some(ClientOpts{content_ref_inline_max_size: 0}),
                locked_components: spin_testing::from_json!([{
                "id": "component1",
                "source": {
                    "content_type": "application/wasm",
                    "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                    "digest": "digest",
                },
                "files": [
                    {
                        "source": format!("file://{}", working_dir.path().join("component1").to_str().unwrap()),
                        "path": working_dir.path().join("component1").join("bar").to_str().unwrap()
                    },
                    {
                        "source": format!("file://{}", working_dir.path().join("component1").to_str().unwrap()),
                        "path": working_dir.path().join("component1").join("baz").to_str().unwrap()
                    }
                ]
                }]),
                expected_layer_count: 3,
                expected_error: None,
            },
            TestCase {
                name: "One component layer and one file with inlined content",
                opts: None,
                locked_components: spin_testing::from_json!([{
                "id": "component1",
                "source": {
                    "content_type": "application/wasm",
                    "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                    "digest": "digest",
                },
                "files": [
                    {
                        "source": format!("file://{}", working_dir.path().join("component1").to_str().unwrap()),
                        "path": working_dir.path().join("component1").join("bar").to_str().unwrap()
                    }
                ]
                }]),
                expected_layer_count: 1,
                expected_error: None,
            },
            TestCase {
                name: "Component has no source",
                opts: None,
                locked_components: spin_testing::from_json!([{
                "id": "component1",
                "source": {
                    "content_type": "application/wasm",
                    "source": "",
                    "digest": "digest",
                }
                }]),
                expected_layer_count: 0,
                expected_error: Some("Invalid URL: \"\""),
            },
            TestCase {
                name: "Duplicate component sources",
                opts: None,
                locked_components: spin_testing::from_json!([{
                    "id": "component1",
                    "source": {
                        "content_type": "application/wasm",
                        "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                        "digest": "digest",
                }},
                {
                    "id": "component2",
                    "source": {
                        "content_type": "application/wasm",
                        "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                        "digest": "digest",
                }}]),
                expected_layer_count: 1,
                expected_error: None,
            },
            TestCase {
                name: "Duplicate file paths",
                opts: Some(ClientOpts{content_ref_inline_max_size: 0}),
                locked_components: spin_testing::from_json!([{
                "id": "component1",
                "source": {
                    "content_type": "application/wasm",
                    "source": format!("file://{}", working_dir.path().join("component1.wasm").to_str().unwrap()),
                    "digest": "digest",
                },
                "files": [
                    {
                        "source": format!("file://{}", working_dir.path().join("component1").to_str().unwrap()),
                        "path": working_dir.path().join("component1").join("bar").to_str().unwrap()
                    },
                    {
                        "source": format!("file://{}", working_dir.path().join("component1").to_str().unwrap()),
                        "path": working_dir.path().join("component1").join("baz").to_str().unwrap()
                    }
                ]},
                {
                    "id": "component2",
                    "source": {
                        "content_type": "application/wasm",
                        "source": format!("file://{}", working_dir.path().join("component2.wasm").to_str().unwrap()),
                        "digest": "digest",
                },
                "files": [
                    {
                        "source": format!("file://{}", working_dir.path().join("component2").to_str().unwrap()),
                        "path": working_dir.path().join("component2").join("baz").to_str().unwrap()
                    }
                ]
                }]),
                expected_layer_count: 4,
                expected_error: None,
            },
        ]
        .to_vec();

        for tc in tests {
            let triggers = Default::default();
            let metadata = Default::default();
            let variables = Default::default();
            let mut locked = LockedApp {
                spin_lock_version: Default::default(),
                components: tc.locked_components,
                triggers,
                metadata,
                variables,
                must_understand: Default::default(),
                host_requirements: Default::default(),
            };

            let mut client = Client::new(false, Some(working_dir.path().to_path_buf()))
                .await
                .expect("should create new client");
            if let Some(o) = tc.opts {
                client.opts = o;
            }

            match tc.expected_error {
                Some(e) => {
                    assert_eq!(
                        e,
                        client
                            .assemble_layers(&mut locked, AssemblyMode::Simple)
                            .await
                            .unwrap_err()
                            .to_string(),
                        "{}",
                        tc.name
                    )
                }
                None => {
                    assert_eq!(
                        tc.expected_layer_count,
                        client
                            .assemble_layers(&mut locked, AssemblyMode::Simple)
                            .await
                            .unwrap()
                            .len(),
                        "{}",
                        tc.name
                    )
                }
            }
        }
    }
}
