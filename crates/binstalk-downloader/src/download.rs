use std::{fmt, io, marker::PhantomData, path::Path};

use binstalk_types::cargo_toml_binstall::PkgFmtDecomposed;
use bytes::Bytes;
use futures_util::{stream::FusedStream, Stream, StreamExt};
use thiserror::Error as ThisError;
use tracing::{debug, error, instrument};

pub use binstalk_types::cargo_toml_binstall::{PkgFmt, TarBasedFmt};

use crate::remote::{Client, Error as RemoteError, Url};

mod async_extracter;
use async_extracter::*;

mod async_tar_visitor;
use async_tar_visitor::extract_tar_based_stream_and_visit;
pub use async_tar_visitor::{TarEntriesVisitor, TarEntry, TarEntryType};

mod extracter;

mod extracted_files;
pub use extracted_files::{ExtractedFiles, ExtractedFilesEntry};

mod zip_extraction;
pub use zip_extraction::ZipError;

#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum DownloadError {
    #[error("Failed to extract zipfile: {0}")]
    Unzip(#[from] ZipError),

    #[error("Failed to download from remote: {0}")]
    Remote(#[from] RemoteError),

    /// A generic I/O error.
    ///
    /// - Code: `binstall::io`
    /// - Exit: 74
    #[error("I/O Error: {0}")]
    Io(io::Error),
}

impl From<io::Error> for DownloadError {
    fn from(err: io::Error) -> Self {
        if err.get_ref().is_some() {
            let kind = err.kind();

            let inner = err
                .into_inner()
                .expect("err.get_ref() returns Some, so err.into_inner() should also return Some");

            inner
                .downcast()
                .map(|b| *b)
                .unwrap_or_else(|err| DownloadError::Io(io::Error::new(kind, err)))
        } else {
            DownloadError::Io(err)
        }
    }
}

impl From<DownloadError> for io::Error {
    fn from(e: DownloadError) -> io::Error {
        match e {
            DownloadError::Io(io_error) => io_error,
            e => io::Error::new(io::ErrorKind::Other, e),
        }
    }
}

pub trait DataVerifier: Send + Sync {
    /// Digest input data.
    ///
    /// This method can be called repeatedly for use with streaming messages,
    /// it will be called in the order of the message received.
    fn update(&mut self, data: &Bytes);
}

impl<T> DataVerifier for T
where
    T: FnMut(&Bytes) + Send + Sync,
{
    fn update(&mut self, data: &Bytes) {
        (*self)(data)
    }
}

pub struct Download<'a> {
    client: Client,
    url: Url,
    data_verifier: Option<&'a mut dyn DataVerifier>,
}

impl fmt::Debug for Download<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(dead_code, clippy::type_complexity)]
        #[derive(Debug)]
        struct Download<'a> {
            client: &'a Client,
            url: &'a Url,
            data_verifier: Option<PhantomData<&'a mut dyn DataVerifier>>,
        }

        fmt::Debug::fmt(
            &Download {
                client: &self.client,
                url: &self.url,
                data_verifier: self.data_verifier.as_ref().map(|_| PhantomData),
            },
            f,
        )
    }
}

impl Download<'static> {
    pub fn new(client: Client, url: Url) -> Self {
        Self {
            client,
            url,
            data_verifier: None,
        }
    }
}

impl<'a> Download<'a> {
    pub fn new_with_data_verifier(
        client: Client,
        url: Url,
        data_verifier: &'a mut dyn DataVerifier,
    ) -> Self {
        Self {
            client,
            url,
            data_verifier: Some(data_verifier),
        }
    }
}

impl<'a> Download<'a> {
    async fn get_stream(
        self,
    ) -> Result<
        impl Stream<Item = Result<Bytes, DownloadError>> + FusedStream + Send + Sync + Unpin + 'a,
        DownloadError,
    > {
        let mut data_verifier = self.data_verifier;
        Ok(self
            .client
            .get_stream(self.url)
            .await?
            .map(move |res| {
                let bytes = res?;

                if let Some(data_verifier) = &mut data_verifier {
                    data_verifier.update(&bytes);
                }

                Ok(bytes)
            })
            // Call `fuse` at the end to make sure `data_verifier` is only
            // called when the stream still has elements left.
            .fuse())
    }
}

/// Make sure `stream` is an alias instead of taking the value to avoid
/// exploding size of the future generated.
///
/// Accept `FusedStream` only since the `stream` could be already consumed.
async fn consume_stream<S>(stream: &mut S)
where
    S: Stream<Item = Result<Bytes, DownloadError>> + FusedStream + Unpin,
{
    while let Some(res) = stream.next().await {
        if let Err(err) = res {
            error!(?err, "failed to consume stream");
            break;
        }
    }
}

impl Download<'_> {
    /// Download a file from the provided URL and process them in memory.
    ///
    /// This does not support verifying a checksum due to the partial extraction
    /// and will ignore one if specified.
    ///
    /// NOTE that this API does not support gnu extension sparse file unlike
    /// [`Download::and_extract`].
    #[instrument(skip(visitor))]
    pub async fn and_visit_tar(
        self,
        fmt: TarBasedFmt,
        visitor: &mut dyn TarEntriesVisitor,
    ) -> Result<(), DownloadError> {
        let has_data_verifier = self.data_verifier.is_some();
        let mut stream = self.get_stream().await?;

        debug!("Downloading and extracting then in-memory processing");

        match extract_tar_based_stream_and_visit(&mut stream, fmt, visitor).await {
            Ok(()) => {
                debug!("Download, extraction and in-memory procession OK");
                Ok(())
            }
            Err(err) => {
                if has_data_verifier {
                    consume_stream(&mut stream).await;
                }
                Err(err)
            }
        }
    }

    /// Download a file from the provided URL and extract it to the provided path.
    ///
    /// NOTE that this would only extract directory and regular files.
    #[instrument(skip(path))]
    pub async fn and_extract(
        self,
        fmt: PkgFmt,
        path: impl AsRef<Path>,
    ) -> Result<ExtractedFiles, DownloadError> {
        async fn inner(
            this: Download<'_>,
            fmt: PkgFmt,
            path: &Path,
        ) -> Result<ExtractedFiles, DownloadError> {
            let has_data_verifier = this.data_verifier.is_some();
            let mut stream = this.get_stream().await?;

            debug!("Downloading and extracting to: '{}'", path.display());

            let res = match fmt.decompose() {
                PkgFmtDecomposed::Tar(fmt) => {
                    extract_tar_based_stream(&mut stream, path, fmt).await
                }
                PkgFmtDecomposed::Bin => extract_bin(&mut stream, path).await,
                PkgFmtDecomposed::Zip => extract_zip(&mut stream, path).await,
            };

            match res {
                Ok(extracted_files) => {
                    debug!("Download OK, extracted to: '{}'", path.display());
                    Ok(extracted_files)
                }
                Err(err) => {
                    if has_data_verifier {
                        consume_stream(&mut stream).await;
                    }
                    Err(err)
                }
            }
        }

        inner(self, fmt, path.as_ref()).await
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        num::NonZeroU16,
    };
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_and_extract() {
        let client = crate::remote::Client::new(
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
            None,
            NonZeroU16::new(10).unwrap(),
            1.try_into().unwrap(),
            [],
        )
        .unwrap();

        // cargo-binstall
        let cargo_binstall_url = "https://github.com/cargo-bins/cargo-binstall/releases/download/v0.20.1/cargo-binstall-aarch64-unknown-linux-musl.tgz";

        let extracted_files =
            Download::new(client.clone(), Url::parse(cargo_binstall_url).unwrap())
                .and_extract(PkgFmt::Tgz, tempdir().unwrap())
                .await
                .unwrap();

        assert!(extracted_files.has_file(Path::new("cargo-binstall")));
        assert!(!extracted_files.has_file(Path::new("1234")));

        let files = HashSet::from([OsStr::new("cargo-binstall").into()]);
        assert_eq!(extracted_files.get_dir(Path::new(".")).unwrap(), &files);

        assert_eq!(
            extracted_files.0,
            HashMap::from([
                (
                    Path::new("cargo-binstall").into(),
                    ExtractedFilesEntry::File
                ),
                (
                    Path::new(".").into(),
                    ExtractedFilesEntry::Dir(Box::new(files))
                )
            ])
        );

        // cargo-watch
        let cargo_watch_url = "https://github.com/watchexec/cargo-watch/releases/download/v8.4.0/cargo-watch-v8.4.0-aarch64-unknown-linux-gnu.tar.xz";

        let extracted_files = Download::new(client.clone(), Url::parse(cargo_watch_url).unwrap())
            .and_extract(PkgFmt::Txz, tempdir().unwrap())
            .await
            .unwrap();

        let dir = Path::new("cargo-watch-v8.4.0-aarch64-unknown-linux-gnu");

        assert_eq!(
            extracted_files.get_dir(Path::new(".")).unwrap(),
            &HashSet::from([dir.as_os_str().into()])
        );

        assert_eq!(
            extracted_files.get_dir(dir).unwrap(),
            &HashSet::from_iter(
                [
                    "README.md",
                    "LICENSE",
                    "completions",
                    "cargo-watch",
                    "cargo-watch.1"
                ]
                .iter()
                .map(OsStr::new)
                .map(Box::<OsStr>::from)
            ),
        );

        assert_eq!(
            extracted_files.get_dir(&dir.join("completions")).unwrap(),
            &HashSet::from([OsStr::new("zsh").into()]),
        );

        assert!(extracted_files.has_file(&dir.join("cargo-watch")));
        assert!(extracted_files.has_file(&dir.join("cargo-watch.1")));
        assert!(extracted_files.has_file(&dir.join("LICENSE")));
        assert!(extracted_files.has_file(&dir.join("README.md")));

        assert!(!extracted_files.has_file(&dir.join("completions")));
        assert!(!extracted_files.has_file(&dir.join("asdfcqwe")));

        assert!(extracted_files.has_file(&dir.join("completions/zsh")));

        // sccache, tgz and zip
        let sccache_config = [
            ("https://github.com/mozilla/sccache/releases/download/v0.3.3/sccache-v0.3.3-x86_64-pc-windows-msvc.tar.gz", PkgFmt::Tgz),
            ("https://github.com/mozilla/sccache/releases/download/v0.3.3/sccache-v0.3.3-x86_64-pc-windows-msvc.zip", PkgFmt::Zip),
        ];

        for (sccache_url, fmt) in sccache_config {
            let extracted_files = Download::new(client.clone(), Url::parse(sccache_url).unwrap())
                .and_extract(fmt, tempdir().unwrap())
                .await
                .unwrap();

            let dir = Path::new("sccache-v0.3.3-x86_64-pc-windows-msvc");

            assert_eq!(
                extracted_files.get_dir(Path::new(".")).unwrap(),
                &HashSet::from([dir.as_os_str().into()])
            );

            assert_eq!(
                extracted_files.get_dir(dir).unwrap(),
                &HashSet::from_iter(
                    ["README.md", "LICENSE", "sccache.exe"]
                        .iter()
                        .map(OsStr::new)
                        .map(Box::<OsStr>::from)
                ),
            );
        }
    }
}
