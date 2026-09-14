use futures_util::stream::BoxStream;
use object_store::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
pub struct Fault {
    pub inner: Arc<dyn ObjectStore>,
    pub mode: AtomicUsize,
    pub puts: AtomicUsize,
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteFaultTestStore")
    }
}
#[async_trait::async_trait]
impl ObjectStore for Fault {
    async fn get_opts(&self, path: &path::Path, opts: GetOptions) -> Result<GetResult> {
        self.inner.get_opts(path, opts).await
    }
    async fn put_opts(
        &self,
        path: &path::Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.load(Ordering::SeqCst);
        if mode != 2 {
            self.inner.put_opts(path, payload, opts).await?;
        }
        Err(Error::Generic {
            store: "fixture",
            source: "lost conditional write response".into(),
        })
    }
    async fn put_multipart_opts(
        &self,
        path: &path::Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, opts).await
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, Result<path::Path>>,
    ) -> BoxStream<'static, Result<path::Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(&self, prefix: Option<&path::Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&path::Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, from: &path::Path, to: &path::Path, opts: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}
