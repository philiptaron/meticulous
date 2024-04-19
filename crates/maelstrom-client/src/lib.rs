pub mod test;

pub use maelstrom_client_base::{spec, ArtifactUploadProgress, MANIFEST_DIR};

use anyhow::{anyhow, Result};
use maelstrom_base::{
    stats::JobStateCounts, ArtifactType, ClientJobId, JobOutcomeResult, JobSpec, Sha256Digest,
};
use maelstrom_client_base::{
    proto::{self, client_process_client::ClientProcessClient},
    IntoProtoBuf, IntoResult, TryFromProtoBuf,
};
use maelstrom_container::ContainerImage;
use maelstrom_util::config::common::{BrokerAddr, CacheSize, InlineLimit, Slots};
use spec::Layer;
use std::{future::Future, os::unix::net::UnixStream, path::Path, pin::Pin, process, thread};

type BoxedFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type RequestFn = Box<
    dyn FnOnce(ClientProcessClient<tonic::transport::Channel>) -> BoxedFuture
        + Send
        + Sync
        + 'static,
>;
type RequestReceiver = tokio::sync::mpsc::UnboundedReceiver<RequestFn>;
type RequestSender = tokio::sync::mpsc::UnboundedSender<RequestFn>;

type TonicResult<T> = std::result::Result<T, tonic::Status>;
type TonicResponse<T> = TonicResult<tonic::Response<T>>;

#[tokio::main]
async fn run_dispatcher(std_sock: UnixStream, mut requester: RequestReceiver) -> Result<()> {
    std_sock.set_nonblocking(true)?;
    let sock = tokio::net::UnixStream::from_std(std_sock.try_clone()?)?;
    let mut closure =
        Some(move || async move { std::result::Result::<_, tower::BoxError>::Ok(sock) });
    let channel = tonic::transport::Endpoint::try_from("http://[::]")?
        .connect_with_connector(tower::service_fn(move |_| {
            (closure.take().expect("unexpected reconnect"))()
        }))
        .await?;

    while let Some(f) = requester.recv().await {
        tokio::spawn(f(ClientProcessClient::new(channel.clone())));
    }

    std_sock.shutdown(std::net::Shutdown::Both)?;

    Ok(())
}

fn print_error(label: &str, res: Result<()>) {
    if let Err(e) = res {
        eprintln!("{label}: error: {e:?}");
    }
}

struct ClientBgHandle(maelstrom_linux::Pid);

impl ClientBgHandle {
    fn wait(&mut self) -> Result<()> {
        maelstrom_linux::waitpid(self.0).map_err(|e| anyhow!("waitpid failed: {e}"))?;
        Ok(())
    }
}

pub struct ClientBgProcess {
    handle: ClientBgHandle,
    sock: Option<UnixStream>,
}

impl ClientBgProcess {
    pub fn new_from_fork() -> Result<Self> {
        let (sock1, sock2) = UnixStream::pair()?;
        if let Some(pid) = maelstrom_linux::fork().map_err(|e| anyhow!("fork failed: {e}"))? {
            Ok(Self {
                handle: ClientBgHandle(pid),
                sock: Some(sock1),
            })
        } else {
            match maelstrom_client_process::main(sock2, None) {
                Ok(()) => process::exit(0),
                Err(err) => {
                    eprintln!("exiting because of error: {err}");
                    process::exit(1);
                }
            }
        }
    }

    fn take_socket(&mut self) -> UnixStream {
        self.sock.take().unwrap()
    }

    fn wait(&mut self) -> Result<()> {
        self.handle.wait()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        drop(self.requester.take());
        print_error(
            "dispatcher",
            self.dispatcher_handle.take().unwrap().join().unwrap(),
        );
        self.process_handle.wait().unwrap();
    }
}

pub struct Client {
    requester: Option<RequestSender>,
    process_handle: ClientBgProcess,
    dispatcher_handle: Option<thread::JoinHandle<Result<()>>>,
    log: slog::Logger,
}

fn flatten_rpc_result<ProtRetT>(res: TonicResponse<ProtRetT>) -> Result<ProtRetT::Output>
where
    ProtRetT: IntoResult,
{
    res?.into_inner().into_result()
}

impl Client {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut process_handle: ClientBgProcess,
        broker_addr: Option<BrokerAddr>,
        project_dir: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
        cache_size: CacheSize,
        inline_limit: InlineLimit,
        slots: Slots,
        log: slog::Logger,
    ) -> Result<Self> {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();

        let sock = process_handle.take_socket();
        let dispatcher_handle = thread::spawn(move || run_dispatcher(sock, recv));
        let s = Self {
            requester: Some(send),
            process_handle,
            dispatcher_handle: Some(dispatcher_handle),
            log,
        };

        slog::debug!(s.log, "client sending start";
            "broker_addr" => ?broker_addr,
            "project_dir" => ?project_dir.as_ref(),
            "cache_dir" => ?cache_dir.as_ref(),
        );
        let msg = proto::StartRequest {
            broker_addr: broker_addr.into_proto_buf(),
            project_dir: project_dir.as_ref().into_proto_buf(),
            cache_dir: cache_dir.as_ref().into_proto_buf(),
            cache_size: cache_size.into_proto_buf(),
            inline_limit: inline_limit.into_proto_buf(),
            slots: slots.into_proto_buf(),
        };
        s.send_sync(|mut client| async move { client.start(msg).await })?;
        slog::debug!(s.log, "client completed start");

        Ok(s)
    }

    fn send_async<BuilderT, FutureT, ProtRetT>(
        &self,
        builder: BuilderT,
    ) -> Result<std::sync::mpsc::Receiver<Result<ProtRetT::Output>>>
    where
        BuilderT: FnOnce(ClientProcessClient<tonic::transport::Channel>) -> FutureT,
        BuilderT: Send + Sync + 'static,
        FutureT:
            Future<Output = std::result::Result<tonic::Response<ProtRetT>, tonic::Status>> + Send,
        ProtRetT: IntoResult,
        ProtRetT::Output: Send + 'static,
    {
        let (send, recv) = std::sync::mpsc::channel();
        self.requester
            .as_ref()
            .unwrap()
            .send(Box::new(move |client| {
                Box::pin(async move {
                    let _ = send.send(flatten_rpc_result(builder(client).await));
                })
            }))?;
        Ok(recv)
    }

    fn send_sync<BuilderT, FutureT, ProtRetT>(&self, builder: BuilderT) -> Result<ProtRetT::Output>
    where
        BuilderT: FnOnce(ClientProcessClient<tonic::transport::Channel>) -> FutureT,
        BuilderT: Send + Sync + 'static,
        FutureT:
            Future<Output = std::result::Result<tonic::Response<ProtRetT>, tonic::Status>> + Send,
        ProtRetT: IntoResult,
        ProtRetT::Output: Send + 'static,
    {
        self.send_async(builder)?.recv()?
    }

    pub fn add_artifact(&self, path: &Path) -> Result<Sha256Digest> {
        slog::debug!(self.log, "client.add_artifact"; "path" => ?path);
        let msg = proto::AddArtifactRequest {
            path: path.into_proto_buf(),
        };
        let digest =
            self.send_sync(move |mut client| async move { client.add_artifact(msg).await })?;
        slog::debug!(self.log, "client.add_artifact complete");
        Ok(digest.try_into()?)
    }

    pub fn add_layer(&self, layer: Layer) -> Result<(Sha256Digest, ArtifactType)> {
        slog::debug!(self.log, "client.add_layer"; "layer" => ?layer);
        let msg = proto::AddLayerRequest {
            layer: Some(layer.into_proto_buf()),
        };
        let spec = self.send_sync(move |mut client| async move { client.add_layer(msg).await })?;
        slog::debug!(self.log, "client.add_layer complete");
        Ok((
            TryFromProtoBuf::try_from_proto_buf(spec.digest)?,
            TryFromProtoBuf::try_from_proto_buf(spec.r#type)?,
        ))
    }

    pub fn get_container_image(&self, name: &str, tag: &str) -> Result<ContainerImage> {
        let msg = proto::GetContainerImageRequest {
            name: name.into(),
            tag: tag.into(),
        };
        let img =
            self.send_sync(move |mut client| async move { client.get_container_image(msg).await })?;
        TryFromProtoBuf::try_from_proto_buf(img)
    }

    pub fn add_job(
        &self,
        spec: JobSpec,
        handler: impl FnOnce(ClientJobId, JobOutcomeResult) + Send + Sync + 'static,
    ) -> Result<()> {
        let msg = proto::AddJobRequest {
            spec: Some(spec.into_proto_buf()),
        };
        self.requester
            .as_ref()
            .unwrap()
            .send(Box::new(move |mut client| {
                Box::pin(async move {
                    let inner = async move {
                        let res = client.add_job(msg).await?.into_inner();
                        let result: proto::JobOutcomeResult =
                            res.result.ok_or(anyhow!("malformed AddJobResponse"))?;
                        Result::<_, anyhow::Error>::Ok((
                            TryFromProtoBuf::try_from_proto_buf(res.client_job_id)?,
                            TryFromProtoBuf::try_from_proto_buf(result)?,
                        ))
                    };
                    if let Ok((cjid, result)) = inner.await {
                        tokio::task::spawn_blocking(move || handler(cjid, result));
                    }
                })
            }))?;
        Ok(())
    }

    pub fn wait_for_outstanding_jobs(&self) -> Result<()> {
        self.send_sync(move |mut client| async move {
            client.wait_for_outstanding_jobs(proto::Void {}).await
        })?;
        Ok(())
    }

    pub fn get_job_state_counts(
        &self,
    ) -> Result<std::sync::mpsc::Receiver<Result<JobStateCounts>>> {
        self.send_async(move |mut client| async move {
            let res = client.get_job_state_counts(proto::Void {}).await?;
            Ok(res.map(|v| TryFromProtoBuf::try_from_proto_buf(v.into_result()?)))
        })
    }

    pub fn get_artifact_upload_progress(&self) -> Result<Vec<ArtifactUploadProgress>> {
        self.send_sync(move |mut client| async move {
            let res = client.get_artifact_upload_progress(proto::Void {}).await?;
            Ok(res.map(|v| TryFromProtoBuf::try_from_proto_buf(v.into_result()?)))
        })
    }
}
