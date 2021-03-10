use std::net::SocketAddr;

use ipnet::Ipv4Net;
use tokio::sync::mpsc;
use warp::Filter;

mod arena;
mod robot;
mod network;
mod webui;
mod optitrack;
mod software;
mod journal;
mod router;

/// TODO:
/// 1. Clean up this code so that it compiles again [DONE]
/// 1a. Quick investigation into what a robot enum (static dispatch) would look like [DONE]
/// 1b. Use static_dir to embedded static resources inside of the app [DONE]
/// 1c. Start experiment triggers upload to all robots? [DONE]
/// 1d. Add basic control software checks, is there one .argos file, are all referenced files included?
/// 1e. Start ARGoS with the controller and shutdown at end of experiment 

#[tokio::main]
async fn main() {
    /* initialize the logger */
    let environment = env_logger::Env::default().default_filter_or("mns_supervisor=info");
    env_logger::Builder::from_env(environment).init();
    /* create a task for tracking the robots and state of the experiment */
    let (arena_requests_tx, arena_requests_rx) = mpsc::unbounded_channel();
    let (network_addr_tx, network_addr_rx) = mpsc::unbounded_channel();
    let (journal_requests_tx, journal_requests_rx) = mpsc::unbounded_channel();
    /* add all network addresses from the 192.168.1.0/24 subnet */
    for network_addr in "192.168.1.0/24".parse::<Ipv4Net>().unwrap().hosts() {
        network_addr_tx.send(network_addr).unwrap();
    }
    let message_router_addr : SocketAddr = ([127, 0, 0, 1], 4950).into();
    /* listen for the ctrl-c shutdown signal */
    let sigint_task = tokio::signal::ctrl_c();
    /* create journal task */
    let journal_task = journal::new(journal_requests_rx);
    /* create arena task */
    let arena_task = arena::new(message_router_addr, arena_requests_rx, &network_addr_tx, &journal_requests_tx);
    /* create network task */
    let network_task = network::new(network_addr_rx, &arena_requests_tx);
    /* create message router task */
    let router_task = router::new(message_router_addr, &journal_requests_tx);
    /* create webui task */
    /* clone arena requests tx for moving into the closure */
    let arena_requests_tx = arena_requests_tx.clone();
    let arena_filter = warp::any().map(move || arena_requests_tx.clone());
    let socket_route = warp::path("socket")
        .and(warp::ws())
        .and(arena_filter)
        .map(|websocket: warp::ws::Ws, arena_requests_tx| {
            websocket.on_upgrade(move |socket| webui::run(socket, arena_requests_tx))
        });
    let static_route = warp::get()
    //    .and(static_dir::static_dir!("static"));
        .and(warp::fs::dir("/home/mallwright/Workspace/mns-supervisor/static"));
    let server_addr : SocketAddr = ([127, 0, 0, 1], 3030).into();
    let webui_task = warp::serve(socket_route.or(static_route)).run(server_addr);
    /* pin the futures so that they can be polled via &mut */
    tokio::pin!(arena_task);
    tokio::pin!(journal_task);
    tokio::pin!(network_task);
    tokio::pin!(router_task);
    tokio::pin!(webui_task);
    tokio::pin!(sigint_task);
    /* open a local browser window */
    /* TODO: implement "reconnecting in javascript", if no client connects within 1 second,
       open new browser window */
    let server_addr = format!("http://{}/", server_addr);
    if let Err(_) = webbrowser::open(&server_addr) {
        log::warn!("Could not start browser");
        log::info!("Please open this URL manually: {}", server_addr);
    }
    /* attempt to complete the futures */
    tokio::select! {
        _ = &mut arena_task => {},
        _ = &mut journal_task => {},
        _ = &mut network_task => {},
        _ = &mut router_task => {},
        _ = &mut webui_task => {},
        _ = &mut sigint_task => {
            /* TODO: is it safe to do this? should messages be broadcast to robots */
            /* what happens if ARGoS is running on the robots, does breaking the
               connection to fernbedienung kill ARGoS? How does the Pixhawk respond */
            log::info!("Shutting down");
        }
    }
}