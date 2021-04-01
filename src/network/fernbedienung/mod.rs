use std::net::Ipv4Addr;
use std::path::PathBuf;

use std::collections::HashMap;

use bytes::BytesMut;
use mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio::sync::{mpsc::{self, UnboundedReceiver}, oneshot};
use uuid::Uuid;
use futures::{self, FutureExt, StreamExt, stream::FuturesUnordered};

use tokio::net::TcpStream;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_serde::{SymmetricallyFramed, formats::SymmetricalJson};
use regex::Regex;

mod protocol;

pub use protocol::{Upload, stream::Stream, process::Run};

lazy_static::lazy_static! {
    static ref REGEX_LINK_STRENGTH: Regex = 
        Regex::new(r"signal:\s+(-\d+)\s+dBm+").unwrap();
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("Could not send request")]
    RequestError,
    #[error("Could not execute request")]
    ExecuteError,
    #[error("Did not receive response")]
    ResponseError,
    #[error("Could not decode data")]
    DecodeError,
}

pub type Result<T> = std::result::Result<T, Error>;

type RemoteResponses = SymmetricallyFramed<
    FramedRead<tokio::io::ReadHalf<TcpStream>, LengthDelimitedCodec>,
    protocol::Response,
    SymmetricalJson<protocol::Response>>;

pub type RemoteRequests = SymmetricallyFramed<
    FramedWrite<tokio::io::WriteHalf<TcpStream>, LengthDelimitedCodec>,
    protocol::Request,
    SymmetricalJson<protocol::Request>>;

pub struct Device {
    request_tx: mpsc::UnboundedSender<Request>,
    pub addr: Ipv4Addr
}

enum Request {
    Run {
        task: protocol::process::Run,
        terminate_rx: Option<oneshot::Receiver<()>>,
        stdin_rx: Option<UnboundedReceiver<BytesMut>>,
        stdout_tx: Option<UnboundedSender<BytesMut>>,
        stderr_tx: Option<UnboundedSender<BytesMut>>,
        result: oneshot::Sender<bool>,
    },
    Upload {
        upload: protocol::Upload,
        result: oneshot::Sender<bool>
    },
    Stream {
        stream: protocol::stream::Stream,
        stop_rx: oneshot::Receiver<()>,
        frames_tx: UnboundedSender<BytesMut>,
        result: oneshot::Sender<bool>
    },
}

impl Device {
    pub async fn new(addr: Ipv4Addr, return_addr_tx: mpsc::UnboundedSender<Ipv4Addr>) -> Result<Self> {
        let stream = TcpStream::connect((addr, 17653)).await
            .map_err(|error| Error::IoError(error))?;
        let (local_request_tx, mut local_request_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            /* requests and responses from remote */
            let (read, write) = tokio::io::split(stream);
            let remote_requests: RemoteRequests = SymmetricallyFramed::new(
                FramedWrite::new(write, LengthDelimitedCodec::new()),
                SymmetricalJson::<protocol::Request>::default(),
            );
            let mut remote_responses: RemoteResponses = SymmetricallyFramed::new(
                FramedRead::new(read, LengthDelimitedCodec::new()),
                SymmetricalJson::<protocol::Response>::default(),
            );
            /* create an mpsc channel to share for remote_requests */
            let (remote_requests_tx, remote_requests_rx) = mpsc::unbounded_channel();           
            let mut forward_remote_requests = UnboundedReceiverStream::new(remote_requests_rx)
                .map(|request| Ok(request))
                .forward(remote_requests);
            /* collections for tracking state */
            let mut status_txs: HashMap<Uuid, UnboundedSender<protocol::ResponseKind>> = Default::default();
            let mut tasks: FuturesUnordered<_> = Default::default();
            /* event loop */
            loop {
                tokio::select! {
                    Some(response) = remote_responses.next() => match response {
                        Ok(protocol::Response(uuid, response)) => {
                            if let Some(uuid) = uuid {
                                if let Some(status_tx) = status_txs.get(&uuid) {
                                    let _ = status_tx.send(response);
                                }
                            }
                            else {
                                log::warn!("Received message without identifier: {:?}", response);
                            }
                        },
                        Err(_) => {
                            log::warn!("Could not deserialize response from remote");
                        }
                    },
                    request = local_request_rx.recv() => match request {
                        Some(request) => {
                            let task = match request {
                                Request::Upload { upload, result } => {
                                    let uuid = Uuid::new_v4();
                                    let request = protocol::RequestKind::Upload(upload);
                                    /* subscribe to updates */
                                    let (upload_status_tx, mut upload_status_rx) = mpsc::unbounded_channel();
                                    status_txs.insert(uuid, upload_status_tx);
                                    /* send the request */
                                    let request_result = remote_requests_tx.send(protocol::Request(uuid, request));
                                    /* process responses */
                                    async move {
                                        let _ = result.send(match request_result {
                                            Ok(_) => match upload_status_rx.recv().await {
                                                Some(status) => matches!(status, protocol::ResponseKind::Ok),
                                                _ => false,
                                            }
                                            _ => false,
                                        });
                                        uuid
                                    }.left_future()
                                },
                                Request::Run { task, terminate_rx, stdin_rx, stdout_tx, stderr_tx, result } => {
                                    let uuid = Uuid::new_v4();
                                    let request = protocol::RequestKind::Process(protocol::process::Request::Run(task));
                                    /* subscribe to updates */
                                    let (run_status_tx, run_status_rx) = mpsc::unbounded_channel();
                                    status_txs.insert(uuid, run_status_tx);
                                    /* send the request */
                                    let request_result = remote_requests_tx.send(protocol::Request(uuid, request));
                                    /* process responses */
                                    let remote_requests_tx = remote_requests_tx.clone();
                                    async move {
                                        match request_result {
                                            Ok(_) => Device::handle_run_request(
                                                uuid, run_status_rx, remote_requests_tx, terminate_rx,
                                                stdin_rx, stdout_tx, stderr_tx, result
                                            ).await,
                                            _ => {
                                                let _ = result.send(false);
                                                uuid
                                            }
                                        }
                                    }.right_future().right_future()
                                },
                                Request::Stream { stream, stop_rx, frames_tx, result } => {
                                    let uuid = Uuid::new_v4();
                                    let request = protocol::RequestKind::Stream(protocol::stream::Request::Stream(stream));
                                    /* subscribe to updates */
                                    let (stream_status_tx, stream_status_rx) = mpsc::unbounded_channel();
                                    status_txs.insert(uuid, stream_status_tx);
                                    /* send the request */
                                    let request_result = remote_requests_tx.send(protocol::Request(uuid, request));
                                    /* process responses */
                                    let remote_requests_tx = remote_requests_tx.clone();
                                    async move {
                                        match request_result {
                                            Ok(_) => Device::handle_stream_request(
                                                uuid, stream_status_rx, remote_requests_tx,
                                                stop_rx, frames_tx, result
                                            ).await,
                                            _ => {
                                                let _ = result.send(false);
                                                uuid
                                            }
                                        }
                                    }.left_future().right_future()
                                }
                            };
                            tasks.push(task);
                        },
                        None => {
                            /* terminate this task when the struct is dropped */
                            let _ = return_addr_tx.send(addr);
                            break
                        },
                    },
                    Some(uuid) = tasks.next() => {
                        status_txs.remove(&uuid);
                    },
                    _ = &mut forward_remote_requests => {}
                }
            }
        });
        Ok(Device { request_tx: local_request_tx, addr })
    }

    async fn handle_stream_request(uuid: Uuid,
                                   mut stream_status_rx: mpsc::UnboundedReceiver<protocol::ResponseKind>,
                                   remote_requests_tx: mpsc::UnboundedSender<protocol::Request>,
                                   stop_rx: oneshot::Receiver<()>,
                                   frames_tx: mpsc::UnboundedSender<BytesMut>,
                                   result_tx: oneshot::Sender<bool>) -> Uuid {
        let mut stop_rx = stop_rx.into_stream();

        loop {
            tokio::select! {
                Some(_) = stop_rx.next() => {
                    let request = protocol::Request(uuid, protocol::RequestKind::Stream(
                        protocol::stream::Request::Stop)
                    );
                    let _ = remote_requests_tx.send(request);
                },
                Some(response) = stream_status_rx.recv() => match response {
                    protocol::ResponseKind::Ok => {},
                    protocol::ResponseKind::Error(error) => 
                        log::error!("Request {}: {}", uuid, error),
                    protocol::ResponseKind::Stream(response) => match response {
                        protocol::stream::Response::Frame(data) => {
                            let _ = frames_tx.send(data);
                        }
                    },
                    /* the unused ok and incorrect responses may be shortcomings of the protocol design */
                    response => log::error!("Protocol error: {:?} is not valid for {}", response, uuid),
                },
                else => break
            }
        }
        /* return the uuid so it can be removed from the hashmap */
        uuid
    }

    async fn handle_run_request(uuid: Uuid,
                                mut run_status_rx: mpsc::UnboundedReceiver<protocol::ResponseKind>,
                                remote_requests_tx: mpsc::UnboundedSender<protocol::Request>,
                                terminate_rx: Option<oneshot::Receiver<()>>,
                                stdin_rx: Option<mpsc::UnboundedReceiver<BytesMut>>,
                                stdout_tx: Option<mpsc::UnboundedSender<BytesMut>>,
                                stderr_tx: Option<mpsc::UnboundedSender<BytesMut>>,
                                exit_status_tx: oneshot::Sender<bool>) -> Uuid {
        let mut terminate_rx = match terminate_rx {
            Some(terminate_rx) => terminate_rx.into_stream().left_stream(),
            None => futures::stream::pending().right_stream(),
        };
        let mut stdin_rx = match stdin_rx {
            Some(stdin_rx) => UnboundedReceiverStream::new(stdin_rx).left_stream(),
            None => futures::stream::pending().right_stream(),
        };

        loop {
            tokio::select! {
                Some(_) = terminate_rx.next() => {
                    let request = protocol::Request(uuid, protocol::RequestKind::Process(
                        protocol::process::Request::Terminate)
                    );
                    let _ = remote_requests_tx.send(request);
                },
                Some(stdin) = stdin_rx.next() => {
                    let request = protocol::Request(uuid, protocol::RequestKind::Process(
                        protocol::process::Request::StandardInput(stdin))
                    );
                    let _ = remote_requests_tx.send(request);

                },
                Some(response) = run_status_rx.recv() => match response {
                    protocol::ResponseKind::Ok => {},
                    protocol::ResponseKind::Error(error) => 
                        log::error!("Request {}: {}", uuid, error),
                    protocol::ResponseKind::Process(response) => match response {
                        protocol::process::Response::Terminated(result) => {
                            let _ = exit_status_tx.send(result);
                            break;
                        },
                        protocol::process::Response::StandardOutput(data) => {
                            if let Some(stdout_tx) = &stdout_tx {
                                let _ = stdout_tx.send(data);
                            }
                        },
                        protocol::process::Response::StandardError(data) => {
                            if let Some(stderr_tx) = &stderr_tx {
                                let _ = stderr_tx.send(data);
                            }
                        },
                    },
                    /* the unused ok and incorrect responses may be shortcomings of the protocol design */
                    response => log::error!("Protocol error: {:?} is not valid for {}", response, uuid),
                },
                else => break
            }
        }
        /* return the uuid so it can be removed from the hashmap */
        uuid
    }

    pub async fn stream(&self, 
                        stream: protocol::stream::Stream,
                        stop_rx: oneshot::Receiver<()>,
                        frames_tx: mpsc::UnboundedSender<BytesMut>,
    ) -> Result<bool> {
        let (result_tx, result_rx) = oneshot::channel();
        let request = Request::Stream{ stream, stop_rx, frames_tx, result: result_tx };
        self.request_tx.send(request).map_err(|_ | Error::RequestError)?;
        result_rx.await.map_err(|_| Error::ResponseError)
    }

    pub async fn upload(&self, path: PathBuf, filename: PathBuf, contents: Vec<u8>) -> Result<bool> {
        let upload = protocol::Upload {
            path, filename, contents,
        };
        let (result_tx, result_rx) = oneshot::channel();
        self.request_tx
            .send(Request::Upload { upload, result: result_tx })
            .map_err(|_ | Error::RequestError)?;
        result_rx.await.map_err(|_| Error::ResponseError)
    }

    pub async fn run(&self,
                     task: protocol::process::Run,
                     terminate_rx: Option<oneshot::Receiver<()>>,
                     stdin_rx: Option<mpsc::UnboundedReceiver<BytesMut>>,
                     stdout_tx: Option<mpsc::UnboundedSender<BytesMut>>,
                     stderr_tx: Option<mpsc::UnboundedSender<BytesMut>>) -> Result<bool> {
        let (result_tx, result_rx) = oneshot::channel();
        let request = Request::Run{ task, terminate_rx, stdin_rx, stdout_tx, stderr_tx, result: result_tx };
        self.request_tx.send(request).map_err(|_ | Error::RequestError)?;
        result_rx.await.map_err(|_| Error::ResponseError)
    }

    pub async fn create_temp_dir(&self) -> Result<String> {
        let task = protocol::process::Run {
            target: "mktemp".into(),
            working_dir: "/tmp".into(),
            args: vec!["-d".to_owned()],
        };
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let stdout_stream = UnboundedReceiverStream::new(stdout_rx);
        let (exit_status, stdout) = tokio::try_join!(
            self.run(task, None, None, Some(stdout_tx), None),
            stdout_stream.concat().map(Result::Ok)
        )?;
        match exit_status {
            true => {
                let temp_dir = std::str::from_utf8(stdout.as_ref())
                    .map_err(|_| Error::DecodeError)?;
                Ok(temp_dir.trim().to_owned())
            },
            false => Err(Error::ExecuteError),
        }
    }

    pub async fn hostname(&self) -> Result<String> {
        let task = protocol::process::Run {
            target: "hostname".into(),
            working_dir: "/tmp".into(),
            args: vec![],
        };
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let stdout_stream = UnboundedReceiverStream::new(stdout_rx);
        let (exit_status, stdout) = tokio::try_join!(
            self.run(task, None, None, Some(stdout_tx), None),
            stdout_stream.concat().map(Result::Ok)
        )?;
        match exit_status {
            true => {
                let hostname = std::str::from_utf8(stdout.as_ref())
                    .map_err(|_| Error::DecodeError)?;
                Ok(hostname.trim().to_owned())
            },
            false => Err(Error::ExecuteError),
        }
    }

    pub async fn halt(&self) -> Result<bool> {
        let task = protocol::process::Run {
            target: "echo".into(),
            working_dir: "/tmp".into(),
            args: vec!["halt".to_owned()],
        };
        self.run(task, None, None, None, None).await
    }

    pub async fn reboot(&self) -> Result<bool> {
        let task = protocol::process::Run {
            target: "echo".into(),
            working_dir: "/tmp".into(),
            args: vec!["reboot".to_owned()],
        };
        self.run(task, None, None, None, None).await
    }

    pub async fn link_strength(&self) -> Result<i32> {
        let task = protocol::process::Run {
            target: "iw".into(),
            working_dir: "/tmp".into(),
            args: vec!["dev".to_owned(), "wlan0".to_owned(), "link".to_owned()],
        };
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let stdout_stream = UnboundedReceiverStream::new(stdout_rx);
        let (exit_status, stdout) = tokio::try_join!(
            self.run(task, None, None, Some(stdout_tx), None),
            stdout_stream.concat().map(Result::Ok)
        )?;
        match exit_status {
            true => {
                let link_info = std::str::from_utf8(stdout.as_ref())
                    .map_err(|_| Error::DecodeError)?;
                REGEX_LINK_STRENGTH.captures(link_info)
                    .and_then(|captures| captures.get(1))
                    .map(|capture| capture.as_str())
                    .ok_or(Error::DecodeError)
                    .and_then(|strength| strength.parse().map_err(|_| Error::DecodeError))
            },
            false => Err(Error::ExecuteError)
        }
    }

    pub async fn fswebcam(&self, device: &str, input: usize, palette: &str, 
                          width: usize, height: usize) -> Result<BytesMut> {
        let task = protocol::process::Run {
            target: "fswebcam".into(),
            working_dir: "/tmp".into(),
            args: vec![
                "--device".to_owned(), device.to_owned(),
                "--input".to_owned(), input.to_string(),
                "--palette".to_owned(), palette.to_owned(),
                "--resolution".to_owned(), format!("{}x{}", width, height),
                "--jpeg".to_owned(), "50".to_owned(),
                "--no-banner".to_owned(),
                "--save".to_owned(), "-".to_owned()
            ],
        };
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let stdout_stream = UnboundedReceiverStream::new(stdout_rx);
        let (exit_status, stdout) = tokio::try_join!(
            self.run(task, None, None, Some(stdout_tx), None),
            stdout_stream.concat().map(Result::Ok),
        )?;
        match exit_status {
            true => Ok(stdout),
            false => Err(Error::ExecuteError)
        }
    }
}

