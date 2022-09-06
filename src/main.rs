use futures_util::future::{select, Either};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use hyper_staticfile::Static;
use hyper_tungstenite::HyperWebsocket;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::LinesStream;

#[derive(structopt::StructOpt)]
struct Args {
    #[structopt(default_value = ".")]
    basedir: PathBuf,
    #[structopt(default_value = "127.0.0.1:3000")]
    addr: SocketAddr,
}

struct ServerState {
    basedir: PathBuf,

    stdin: broadcast::Sender<String>,
}

impl ServerState {
    async fn watch_stdin(&self) -> std::io::Result<()> {
        use tokio_stream::StreamExt;

        let stdin_lines = BufReader::new(tokio::io::stdin()).lines();
        let mut stdin_lines = LinesStream::new(stdin_lines);
        while let Some(line) = stdin_lines.next().await {
            let l: String = line?;
            let _ = self.stdin.send(l);
        }
        Ok(())
    }
}

async fn handle_impl(
    mut req: Request<Body>,
    ss: Arc<ServerState>,
) -> Result<Response<Body>, anyhow::Error> {
    let st = Static::new(ss.basedir.clone());

    if hyper_tungstenite::is_upgrade_request(&req) {
        let (response, websocket) = hyper_tungstenite::upgrade(&mut req, None)?;
        tokio::spawn(async move {
            if let Err(e) = serve_websocket(websocket, ss).await {
                eprintln!("Error in websocket connection: {}", e);
            }
        });
        Ok(response)
    } else {
        Ok(st.serve(req).await?)
    }
}

async fn handle(req: Request<Body>, ss: Arc<ServerState>) -> Result<Response<Body>, Infallible> {
    match handle_impl(req, ss).await {
        Ok(k) => Ok(k),
        Err(e) => Ok(Response::builder()
            .status(400)
            .body(Body::from(format!("{e}")))
            .unwrap()),
    }
}

async fn serve_websocket(
    websocket: HyperWebsocket,
    ss: Arc<ServerState>,
) -> Result<(), anyhow::Error> {
    use futures_util::sink::SinkExt;
    use hyper_tungstenite::tungstenite::Message;

    let mut websocket = websocket.await?;

    let mut messaes_from_stdin = ss.stdin.subscribe();

    loop {
        let message = match messaes_from_stdin.recv().await {
            Ok(message) => message,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        websocket.send(Message::Text(message)).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::from_args();

    let (stdin, _) = broadcast::channel(1024);
    let ss = Arc::new(ServerState {
        basedir: args.basedir,
        stdin,
    });

    let ssc = Arc::clone(&ss);
    let handler = move |req| handle(req, Arc::clone(&ssc));
    let sf = service_fn(handler);

    let make_svc = make_service_fn(move |_conn| {
        let sf = sf.clone();
        async { Ok::<_, Infallible>(sf) }
    });

    eprintln!(
        "about to listen on http://{addr} and ws://{addr}",
        addr = &args.addr
    );
    let server = Server::bind(&args.addr).serve(make_svc);
    tokio::pin!(server);
    let watch_stdin = async move { ss.watch_stdin().await };
    tokio::pin!(watch_stdin);

    match select(watch_stdin, server).await {
        Either::Left((l, _)) => {
            // graceful shutdown is not implemented
            l?;
        }
        Either::Right((r, _)) => {
            r?;
        }
    };

    Ok(())
}
