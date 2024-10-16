//! # Read and write to OCI image layout directories
//!
//! This library contains medium and low-level APIs for working with
//! [OCI images], which are basically a directory with blobs and JSON files
//! for metadata.
//!
//! ## Dependency on cap-std
//!
//! This library makes use of [cap-std] to operate in a capability-oriented
//! fashion. In practice, the code in this project is well tested and would
//! not traverse outside its own path root. However, using capabilities
//! is a generally good idea when operating in the container ecosystem,
//! in particular when actively processing tar streams.
//!
//! ## Getting started
//!
//! To access an existing OCI directory:
//!
//! ```rust,no_run
//! # use ocidir::cap_std;
//! # fn main() -> anyhow::Result<()> {
//! let d = cap_std::fs::Dir::open_ambient_dir("/path/to/ocidir", cap_std::ambient_authority())?;
//! let d = ocidir::OciDir::open(&d)?;
//! println!("{:?}", d.read_manifest()?);
//! # Ok(())
//! # }
//! ```
//!
//! Users of this crate are likely to want to perform low-level manipulations
//! such as synthesizing tar layers; [`OciDir::push_layer`] for example can
//! be used for this.
//!
//! [cap-std]: https://docs.rs/cap-std/
//! [OCI images]: https://github.com/opencontainers/image-spec
//!

use cap_std::fs::{Dir, DirBuilderExt};
use cap_std_ext::cap_tempfile;
use cap_std_ext::dirext::CapStdExtDirExt;
use flate2::write::GzEncoder;
use oci_image::MediaType;
use oci_spec::image::{
    self as oci_image, Descriptor, Digest, ImageConfiguration, ImageIndex, ImageManifest,
    Sha256Digest,
};
use olpc_cjson::CanonicalFormatter;
use openssl::hash::{Hasher, MessageDigest};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::fs::File;
use std::io::{prelude::*, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

// Re-export our dependencies that are used as part of the public API.
pub use cap_std_ext::cap_std;
pub use oci_spec;

/// Path inside an OCI directory to the blobs
const BLOBDIR: &str = "blobs/sha256";

const OCI_TAG_ANNOTATION: &str = "org.opencontainers.image.ref.name";

/// Errors returned by this crate.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("i/o error")]
    /// An input/output error
    Io(#[from] std::io::Error),
    #[error("serialization error")]
    /// Returned when serialization or deserialization fails
    SerDe(#[from] serde_json::Error),
    #[error("parsing OCI value")]
    /// Returned when an OCI spec error occurs
    OciSpecError(#[from] oci_spec::OciSpecError),
    #[error("unexpected cryptographic routine error")]
    /// Returned when a cryptographic routine encounters an unexpected problem
    CryptographicError(Box<str>),
    #[error("Expected digest {expected} but found {found}")]
    /// Returned when a digest does not match
    DigestMismatch { expected: Box<str>, found: Box<str> },
    #[error("Expected size {expected} but found {found}")]
    /// Returned when a descriptor digest does not match what was expected
    SizeMismatch { expected: u64, found: u64 },
    #[error("Expected digest algorithm sha256 but found {found}")]
    /// Returned when a digest algorithm is not supported
    UnsupportedDigestAlgorithm { found: Box<str> },
    #[error("error")]
    /// An unknown other error
    Other(Box<str>),
}

/// The error type returned from this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl From<openssl::error::Error> for Error {
    fn from(value: openssl::error::Error) -> Self {
        Self::CryptographicError(value.to_string().into())
    }
}

impl From<openssl::error::ErrorStack> for Error {
    fn from(value: openssl::error::ErrorStack) -> Self {
        Self::CryptographicError(value.to_string().into())
    }
}

/// Completed blob metadata
#[derive(Debug)]
pub struct Blob {
    /// SHA-256 digest
    pub sha256: oci_image::Sha256Digest,
    /// Size
    pub size: u64,
}

impl Blob {
    /// The SHA-256 digest for this blob
    pub fn sha256(&self) -> &oci_image::Sha256Digest {
        &self.sha256
    }

    /// Descriptor
    pub fn descriptor(&self) -> oci_image::DescriptorBuilder {
        oci_image::DescriptorBuilder::default()
            .digest(self.sha256.clone())
            .size(self.size)
    }
}

/// Completed layer metadata
#[derive(Debug)]
pub struct Layer {
    /// The underlying blob (usually compressed)
    pub blob: Blob,
    /// The uncompressed digest, which will be used for "diffid"s
    pub uncompressed_sha256: Sha256Digest,
}

impl Layer {
    /// Return the descriptor for this layer
    pub fn descriptor(&self) -> oci_image::DescriptorBuilder {
        self.blob.descriptor().media_type(MediaType::ImageLayerGzip)
    }

    /// Return a Digest instance for the uncompressed SHA-256.
    pub fn uncompressed_sha256_as_digest(&self) -> Digest {
        self.uncompressed_sha256.clone().into()
    }
}

/// Create an OCI blob.
pub struct BlobWriter<'a> {
    /// Compute checksum
    pub hash: Hasher,
    /// Target file
    pub target: Option<cap_tempfile::TempFile<'a>>,
    size: u64,
}

impl<'a> Debug for BlobWriter<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobWriter")
            .field("target", &self.target)
            .field("size", &self.size)
            .finish()
    }
}

/// Create an OCI tar+gzip layer.
pub struct GzipLayerWriter<'a> {
    bw: BlobWriter<'a>,
    uncompressed_hash: Hasher,
    compressor: GzEncoder<Vec<u8>>,
}

impl<'a> Debug for GzipLayerWriter<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GzipLayerWriter")
            .field("bw", &self.bw)
            .field("compressor", &self.compressor)
            .finish()
    }
}

#[derive(Debug)]
/// An opened OCI directory.
pub struct OciDir {
    /// The underlying directory.
    pub dir: std::sync::Arc<Dir>,
}

/// Write a serializable data (JSON) as an OCI blob
#[deprecated = "Use OciDir::write_json_blob instead"]
pub fn write_json_blob<S: serde::Serialize>(
    ocidir: &Dir,
    v: &S,
    media_type: oci_image::MediaType,
) -> Result<oci_image::DescriptorBuilder> {
    let mut w = BlobWriter::new(ocidir)?;
    let mut ser = serde_json::Serializer::with_formatter(&mut w, CanonicalFormatter::new());
    v.serialize(&mut ser)?;
    let blob = w.complete()?;
    Ok(blob.descriptor().media_type(media_type))
}

/// Create a dummy config descriptor.
/// Our API right now always mutates a manifest, which means we need
/// a "valid" manifest, which requires a "valid" config descriptor.
/// This digest should never actually be used for anything.
fn empty_config_descriptor() -> oci_image::Descriptor {
    oci_image::DescriptorBuilder::default()
        .media_type(MediaType::ImageConfig)
        .size(7023u64)
        .digest(
            Sha256Digest::from_str(
                "a5b2b2c507a0944348e0303114d8d93aaaa081732b86451d9bce1f432a537bc7",
            )
            .unwrap(),
        )
        .build()
        .unwrap()
}

/// Generate a "valid" empty manifest.  See above.
pub fn new_empty_manifest() -> oci_image::ImageManifestBuilder {
    oci_image::ImageManifestBuilder::default()
        .schema_version(oci_image::SCHEMA_VERSION)
        .config(empty_config_descriptor())
        .layers(Vec::new())
}

fn sha256_of_descriptor(desc: &Descriptor) -> Result<&str> {
    desc.as_digest_sha256()
        .ok_or_else(|| Error::UnsupportedDigestAlgorithm {
            found: desc.digest().to_string().into(),
        })
}

impl OciDir {
    /// Open the OCI directory at the target path; if it does not already
    /// have the standard OCI metadata, it is created.
    pub fn ensure(dir: &Dir) -> Result<Self> {
        let mut db = cap_std::fs::DirBuilder::new();
        db.recursive(true).mode(0o755);
        dir.ensure_dir_with(BLOBDIR, &db)?;
        if !dir.try_exists("oci-layout")? {
            dir.atomic_write("oci-layout", r#"{"imageLayoutVersion":"1.0.0"}"#)?;
        }
        Self::open(dir)
    }

    /// Clone an OCI directory, using reflinks for blobs.
    pub fn clone_to(&self, destdir: &Dir, p: impl AsRef<Path>) -> Result<Self> {
        let p = p.as_ref();
        destdir.create_dir(p)?;
        let cloned = Self::ensure(&destdir.open_dir(p)?)?;
        for blob in self.dir.read_dir(BLOBDIR)? {
            let blob = blob?;
            let path = Path::new(BLOBDIR).join(blob.file_name());
            let mut src = self.dir.open(&path).map(BufReader::new)?;
            self.dir
                .atomic_replace_with(&path, |w| std::io::copy(&mut src, w))?;
        }
        Ok(cloned)
    }

    /// Open an existing OCI directory.
    pub fn open(dir: &Dir) -> Result<Self> {
        let dir = std::sync::Arc::new(dir.try_clone()?);
        Ok(Self { dir })
    }

    /// Write a serializable data (JSON) as an OCI blob
    pub fn write_json_blob<S: serde::Serialize>(
        &self,
        v: &S,
        media_type: oci_image::MediaType,
    ) -> Result<oci_image::DescriptorBuilder> {
        // Forward to the legacy implementation; TODO semver for 0.3, drop
        // the legacy API and move it here.
        #[allow(deprecated)]
        write_json_blob(&self.dir, v, media_type)
    }

    /// Create a blob (can be anything).
    pub fn create_blob(&self) -> Result<BlobWriter> {
        BlobWriter::new(&self.dir)
    }

    /// Create a writer for a new gzip+tar blob; the contents
    /// are not parsed, but are expected to be a tarball.
    pub fn create_gzip_layer(&self, c: Option<flate2::Compression>) -> Result<GzipLayerWriter> {
        GzipLayerWriter::new(&self.dir, c)
    }

    /// Create a tar output stream, backed by a blob
    pub fn create_layer(
        &self,
        c: Option<flate2::Compression>,
    ) -> Result<tar::Builder<GzipLayerWriter>> {
        Ok(tar::Builder::new(self.create_gzip_layer(c)?))
    }

    /// Add a layer to the top of the image stack.  The firsh pushed layer becomes the root.

    pub fn push_layer(
        &self,
        manifest: &mut oci_image::ImageManifest,
        config: &mut oci_image::ImageConfiguration,
        layer: Layer,
        description: &str,
        annotations: Option<HashMap<String, String>>,
    ) {
        self.push_layer_annotated(manifest, config, layer, annotations, description);
    }

    /// Add a layer to the top of the image stack with optional annotations.
    ///
    /// This is otherwise equivalent to [`Self::push_layer`].
    pub fn push_layer_annotated(
        &self,
        manifest: &mut oci_image::ImageManifest,
        config: &mut oci_image::ImageConfiguration,
        layer: Layer,
        annotations: Option<impl Into<HashMap<String, String>>>,
        description: &str,
    ) {
        let created = chrono::offset::Utc::now();
        self.push_layer_full(manifest, config, layer, annotations, description, created)
    }

    /// Add a layer to the top of the image stack with optional annotations and desired timestamp.
    ///
    /// This is otherwise equivalent to [`Self::push_layer_annotated`].
    pub fn push_layer_full(
        &self,
        manifest: &mut oci_image::ImageManifest,
        config: &mut oci_image::ImageConfiguration,
        layer: Layer,
        annotations: Option<impl Into<HashMap<String, String>>>,
        description: &str,
        created: chrono::DateTime<chrono::Utc>,
    ) {
        let mut builder = layer.descriptor();
        if let Some(annotations) = annotations {
            builder = builder.annotations(annotations);
        }
        let blobdesc = builder.build().unwrap();
        manifest.layers_mut().push(blobdesc);
        let mut rootfs = config.rootfs().clone();
        rootfs
            .diff_ids_mut()
            .push(layer.uncompressed_sha256_as_digest().to_string());
        config.set_rootfs(rootfs);
        let h = oci_image::HistoryBuilder::default()
            .created(created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .created_by(description.to_string())
            .build()
            .unwrap();
        config.history_mut().push(h);
    }

    fn parse_descriptor_to_path(desc: &oci_spec::image::Descriptor) -> Result<PathBuf> {
        let digest = sha256_of_descriptor(desc)?;
        Ok(Path::new(BLOBDIR).join(digest))
    }

    /// Open a blob; its size is validated as a sanity check.
    pub fn read_blob(&self, desc: &oci_spec::image::Descriptor) -> Result<File> {
        let path = Self::parse_descriptor_to_path(desc)?;
        let f = self.dir.open(path).map(|f| f.into_std())?;
        let expected: u64 = desc.size();
        let found = f.metadata()?.len();
        if expected != found {
            return Err(Error::SizeMismatch { expected, found });
        }
        Ok(f)
    }

    /// Returns `true` if the blob with this digest is already present.
    pub fn has_blob(&self, desc: &oci_spec::image::Descriptor) -> Result<bool> {
        let path = Self::parse_descriptor_to_path(desc)?;
        self.dir.try_exists(path).map_err(Into::into)
    }

    /// Returns `true` if the manifest is already present.
    pub fn has_manifest(&self, desc: &oci_spec::image::Descriptor) -> Result<bool> {
        let Some(index) = self.read_index()? else {
            return Ok(false);
        };
        Ok(index
            .manifests()
            .iter()
            .any(|m| m.digest() == desc.digest()))
    }

    /// Read a JSON blob.
    pub fn read_json_blob<T: serde::de::DeserializeOwned>(
        &self,
        desc: &oci_spec::image::Descriptor,
    ) -> Result<T> {
        let blob = BufReader::new(self.read_blob(desc)?);
        serde_json::from_reader(blob).map_err(Into::into)
    }

    /// Write a configuration blob.
    pub fn write_config(
        &self,
        config: oci_image::ImageConfiguration,
    ) -> Result<oci_image::Descriptor> {
        Ok(self
            .write_json_blob(&config, MediaType::ImageConfig)?
            .build()
            .unwrap())
    }

    /// Read the image index.
    pub fn read_index(&self) -> Result<Option<ImageIndex>> {
        let r = if let Some(index) = self.dir.open_optional("index.json")?.map(BufReader::new) {
            Some(oci_image::ImageIndex::from_reader(index)?)
        } else {
            None
        };
        Ok(r)
    }

    /// Write a manifest as a blob, and replace the index with a reference to it.
    pub fn insert_manifest(
        &self,
        manifest: oci_image::ImageManifest,
        tag: Option<&str>,
        platform: oci_image::Platform,
    ) -> Result<Descriptor> {
        let mut manifest = self
            .write_json_blob(&manifest, MediaType::ImageManifest)?
            .platform(platform)
            .build()
            .unwrap();
        if let Some(tag) = tag {
            let annotations: HashMap<_, _> = [(OCI_TAG_ANNOTATION.to_string(), tag.to_string())]
                .into_iter()
                .collect();
            manifest.set_annotations(Some(annotations));
        }

        let index = self.read_index()?;
        let index = if let Some(mut index) = index {
            let mut manifests = index.manifests().clone();
            if let Some(tag) = tag {
                manifests.retain(|d| !Self::descriptor_is_tagged(d, tag));
            }
            manifests.push(manifest.clone());
            index.set_manifests(manifests);
            index
        } else {
            oci_image::ImageIndexBuilder::default()
                .schema_version(oci_image::SCHEMA_VERSION)
                .manifests(vec![manifest.clone()])
                .build()
                .unwrap()
        };

        self.dir
            .atomic_replace_with("index.json", |mut w| -> Result<()> {
                let mut ser =
                    serde_json::Serializer::with_formatter(&mut w, CanonicalFormatter::new());
                index.serialize(&mut ser)?;
                Ok(())
            })?;
        Ok(manifest)
    }

    /// Convenience helper to write the provided config, update the manifest to use it, then call [`insert_manifest`].
    pub fn insert_manifest_and_config(
        &self,
        mut manifest: oci_image::ImageManifest,
        config: oci_image::ImageConfiguration,
        tag: Option<&str>,
        platform: oci_image::Platform,
    ) -> Result<Descriptor> {
        let config = self.write_config(config)?;
        manifest.set_config(config);
        self.insert_manifest(manifest, tag, platform)
    }

    /// Write a manifest as a blob, and replace the index with a reference to it.
    pub fn replace_with_single_manifest(
        &self,
        manifest: oci_image::ImageManifest,
        platform: oci_image::Platform,
    ) -> Result<()> {
        let manifest = self
            .write_json_blob(&manifest, MediaType::ImageManifest)?
            .platform(platform)
            .build()
            .unwrap();

        let index_data = oci_image::ImageIndexBuilder::default()
            .schema_version(oci_image::SCHEMA_VERSION)
            .manifests(vec![manifest])
            .build()
            .unwrap();
        self.dir
            .atomic_replace_with("index.json", |mut w| -> Result<()> {
                let mut ser =
                    serde_json::Serializer::with_formatter(&mut w, CanonicalFormatter::new());
                index_data.serialize(&mut ser)?;
                Ok(())
            })?;
        Ok(())
    }

    fn descriptor_is_tagged(d: &Descriptor, tag: &str) -> bool {
        d.annotations()
            .as_ref()
            .and_then(|annos| annos.get(OCI_TAG_ANNOTATION))
            .filter(|tagval| tagval.as_str() == tag)
            .is_some()
    }

    /// Find the manifest with the provided tag
    pub fn find_manifest_with_tag(&self, tag: &str) -> Result<Option<oci_image::ImageManifest>> {
        let f = self.dir.open("index.json")?;
        let idx: oci_image::ImageIndex = serde_json::from_reader(BufReader::new(f))?;
        for img in idx.manifests() {
            if Self::descriptor_is_tagged(img, tag) {
                return self.read_json_blob(img).map(Some);
            }
        }
        Ok(None)
    }

    /// Verify a single manifest and all of its referenced objects.
    /// Skips already validated blobs referenced by digest in `validated`,
    /// and updates that set with ones we did validate.
    fn fsck_one_manifest(
        &self,
        manifest: &ImageManifest,
        validated: &mut HashSet<Box<str>>,
    ) -> Result<()> {
        let config_digest = sha256_of_descriptor(manifest.config())?;
        let _: ImageConfiguration = self.read_json_blob(manifest.config())?;
        validated.insert(config_digest.into());
        for layer in manifest.layers() {
            let expected = sha256_of_descriptor(layer)?;
            if validated.contains(expected) {
                continue;
            }
            let mut f = self.read_blob(layer)?;
            let mut digest = Hasher::new(MessageDigest::sha256())?;
            std::io::copy(&mut f, &mut digest)?;
            let found = hex::encode(
                digest
                    .finish()
                    .map_err(|e| Error::Other(e.to_string().into()))?,
            );
            if expected != found {
                return Err(Error::DigestMismatch {
                    expected: expected.into(),
                    found: found.into(),
                });
            }
            validated.insert(expected.into());
        }
        Ok(())
    }

    /// Verify consistency of the index, its manifests, the config and blobs (all the latter)
    /// by verifying their descriptor.
    pub fn fsck(&self) -> Result<u64> {
        let Some(index) = self.read_index()? else {
            return Ok(0);
        };
        let mut validated_blobs = HashSet::new();
        for manifest_descriptor in index.manifests() {
            let expected_sha256 = sha256_of_descriptor(manifest_descriptor)?;
            let manifest: ImageManifest = self.read_json_blob(manifest_descriptor)?;
            validated_blobs.insert(expected_sha256.into());
            self.fsck_one_manifest(&manifest, &mut validated_blobs)?;
        }
        Ok(validated_blobs.len().try_into().unwrap())
    }
}

impl<'a> BlobWriter<'a> {
    fn new(ocidir: &'a Dir) -> Result<Self> {
        Ok(Self {
            hash: Hasher::new(MessageDigest::sha256())?,
            // FIXME add ability to choose filename after completion
            target: Some(cap_tempfile::TempFile::new(ocidir)?),
            size: 0,
        })
    }

    /// Finish writing this blob, verifying its digest and size against the expected descriptor.
    pub fn complete_verified_as(mut self, descriptor: &Descriptor) -> Result<Blob> {
        let expected_digest = sha256_of_descriptor(descriptor)?;
        let found_digest = hex::encode(self.hash.finish()?);
        if found_digest.as_str() != expected_digest {
            return Err(Error::DigestMismatch {
                expected: expected_digest.into(),
                found: found_digest.into(),
            });
        }
        let descriptor_size: u64 = descriptor.size();
        if self.size != descriptor_size {
            return Err(Error::SizeMismatch {
                expected: descriptor_size,
                found: self.size,
            });
        }
        self.complete()
    }

    /// Finish writing this blob object.
    pub fn complete(mut self) -> Result<Blob> {
        let sha256 = hex::encode(self.hash.finish()?);
        let destname = &format!("{}/{}", BLOBDIR, sha256);
        let target = self.target.take().unwrap();
        target.replace(destname)?;
        Ok(Blob {
            sha256: Sha256Digest::from_str(&sha256).unwrap(),
            size: self.size,
        })
    }
}

impl<'a> std::io::Write for BlobWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.hash.update(srcbuf)?;
        self.target
            .as_mut()
            .unwrap()
            .as_file_mut()
            .write_all(srcbuf)?;
        self.size += srcbuf.len() as u64;
        Ok(srcbuf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> GzipLayerWriter<'a> {
    /// Create a writer for a gzip compressed layer blob.
    fn new(ocidir: &'a Dir, c: Option<flate2::Compression>) -> Result<Self> {
        let bw = BlobWriter::new(ocidir)?;
        Ok(Self {
            bw,
            uncompressed_hash: Hasher::new(MessageDigest::sha256())?,
            compressor: GzEncoder::new(Vec::with_capacity(8192), c.unwrap_or_default()),
        })
    }

    /// Consume this writer, flushing buffered data and put the blob in place.
    pub fn complete(mut self) -> Result<Layer> {
        self.compressor.get_mut().clear();
        let buf = self.compressor.finish()?;
        self.bw.write_all(&buf)?;
        let blob = self.bw.complete()?;
        let uncompressed_sha256 =
            Sha256Digest::from_str(&hex::encode(self.uncompressed_hash.finish()?)).unwrap();
        Ok(Layer {
            blob,
            uncompressed_sha256,
        })
    }
}

impl<'a> std::io::Write for GzipLayerWriter<'a> {
    fn write(&mut self, srcbuf: &[u8]) -> std::io::Result<usize> {
        self.compressor.get_mut().clear();
        self.compressor.write_all(srcbuf).unwrap();
        self.uncompressed_hash.update(srcbuf)?;
        let compressed_buf = self.compressor.get_mut().as_slice();
        self.bw.write_all(compressed_buf)?;
        Ok(srcbuf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.bw.flush()
    }
}

#[cfg(test)]
mod tests {
    use cap_std::fs::OpenOptions;

    use super::*;

    const MANIFEST_DERIVE: &str = r#"{
        "schemaVersion": 2,
        "config": {
          "mediaType": "application/vnd.oci.image.config.v1+json",
          "digest": "sha256:54977ab597b345c2238ba28fe18aad751e5c59dc38b9393f6f349255f0daa7fc",
          "size": 754
        },
        "layers": [
          {
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": "sha256:ee02768e65e6fb2bb7058282338896282910f3560de3e0d6cd9b1d5985e8360d",
            "size": 5462
          },
          {
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": "sha256:d203cef7e598fa167cb9e8b703f9f20f746397eca49b51491da158d64968b429",
            "size": 214
          }
        ],
        "annotations": {
          "ostree.commit": "3cb6170b6945065c2475bc16d7bebcc84f96b4c677811a6751e479b89f8c3770",
          "ostree.version": "42.0"
        }
      }
    "#;

    #[test]
    fn manifest() -> Result<()> {
        let m: oci_image::ImageManifest = serde_json::from_str(MANIFEST_DERIVE)?;
        assert_eq!(
            m.layers()[0].digest().to_string(),
            "sha256:ee02768e65e6fb2bb7058282338896282910f3560de3e0d6cd9b1d5985e8360d"
        );
        Ok(())
    }

    #[test]
    fn test_build() -> Result<()> {
        let td = cap_tempfile::tempdir(cap_std::ambient_authority())?;
        let w = OciDir::ensure(&td)?;
        let mut layerw = w.create_gzip_layer(None)?;
        layerw.write_all(b"pretend this is a tarball")?;
        let root_layer = layerw.complete()?;
        let root_layer_desc = root_layer.descriptor().build().unwrap();
        assert_eq!(
            root_layer.uncompressed_sha256.digest(),
            "349438e5faf763e8875b43de4d7101540ef4d865190336c2cc549a11f33f8d7c"
        );
        // Nothing referencing this blob yet
        assert_eq!(w.fsck().unwrap(), 0);
        assert!(w.has_blob(&root_layer_desc).unwrap());

        // Check that we don't find nonexistent blobs
        assert!(!w
            .has_blob(&Descriptor::new(
                MediaType::ImageLayerGzip,
                root_layer.blob.size,
                root_layer.uncompressed_sha256.clone()
            ))
            .unwrap());

        let mut manifest = new_empty_manifest().build().unwrap();
        let mut config = oci_image::ImageConfigurationBuilder::default()
            .build()
            .unwrap();
        let annotations: Option<HashMap<String, String>> = None;
        w.push_layer(&mut manifest, &mut config, root_layer, "root", annotations);
        let config = w.write_config(config)?;
        manifest.set_config(config);
        w.replace_with_single_manifest(manifest.clone(), oci_image::Platform::default())?;
        assert_eq!(w.read_index().unwrap().unwrap().manifests().len(), 1);
        assert_eq!(w.fsck().unwrap(), 3);
        // Also verify that corrupting a blob is found
        {
            let root_layer_sha256 = root_layer_desc.as_digest_sha256().unwrap();
            let mut f = w.dir.open_with(
                format!("blobs/sha256/{root_layer_sha256}"),
                OpenOptions::new().write(true),
            )?;
            let l = f.metadata()?.len();
            f.seek(std::io::SeekFrom::End(0))?;
            f.write_all(b"\0")?;
            assert!(w.fsck().is_err());
            f.set_len(l)?;
            assert_eq!(w.fsck().unwrap(), 3);
        }

        let idx = w.read_index()?.unwrap();
        let manifest_desc = idx.manifests().first().unwrap();
        let read_manifest = w.read_json_blob(manifest_desc).unwrap();
        assert_eq!(&read_manifest, &manifest);

        let desc: Descriptor =
            w.insert_manifest(manifest, Some("latest"), oci_image::Platform::default())?;
        assert!(w.has_manifest(&desc).unwrap());
        // There's more than one now
        assert_eq!(w.read_index().unwrap().unwrap().manifests().len(), 2);

        assert!(w.find_manifest_with_tag("noent").unwrap().is_none());
        let found_via_tag = w.find_manifest_with_tag("latest").unwrap().unwrap();
        assert_eq!(found_via_tag, read_manifest);

        let mut layerw = w.create_gzip_layer(None)?;
        layerw.write_all(b"pretend this is an updated tarball")?;
        let root_layer = layerw.complete()?;
        let mut manifest = new_empty_manifest().build().unwrap();
        let mut config = oci_image::ImageConfigurationBuilder::default()
            .build()
            .unwrap();
        w.push_layer(&mut manifest, &mut config, root_layer, "root", None);
        let _: Descriptor = w.insert_manifest_and_config(
            manifest,
            config,
            Some("latest"),
            oci_image::Platform::default(),
        )?;
        assert_eq!(w.read_index().unwrap().unwrap().manifests().len(), 2);
        assert_eq!(w.fsck().unwrap(), 6);
        Ok(())
    }
}
