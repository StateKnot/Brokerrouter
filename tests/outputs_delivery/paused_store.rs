use futures_util::stream::BoxStream;
use object_store::*;
use std::sync::Arc;

#[derive(Debug)]
pub struct Paused {
    pub inner: Arc<dyn ObjectStore>,
    pub entered: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
}
impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PausedTestStore")
    }
}
#[async_trait::async_trait]
impl ObjectStore for Paused {
    async fn get_opts(&self, path: &path::Path, opts: GetOptions) -> Result<GetResult> {
        let result = self.inner.get_opts(path, opts).await?;
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(result)
    }
    async fn put_opts(
        &self,
        path: &path::Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.inner.put_opts(path, payload, opts).await
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
