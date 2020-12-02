use warp::ws;

use std::{
    collections::HashMap,
    time::Duration
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};

use tokio::{
    sync::mpsc,
    time::timeout,
};

use regex::Regex;

use super::{
    Robots,
    robots,
    Experiment,
    experiment,
    optitrack,
    firmware,
    robots::{
        drone,
        pipuck,
    },   
};

use serde::{Deserialize, Serialize};

use log;

use itertools::Itertools;

/// MDL HTML for icons
const OK_ICON: &str = "<i class=\"material-icons mdl-list__item-icon\" style=\"color:green;\">check_circle</i>";
const ERROR_ICON: &str = "<i class=\"material-icons mdl-list__item-icon\" style=\"color:red;\">error</i>";


#[derive(Serialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Content {
    Text(String),
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>
    },
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Bad request")]
    BadRequest,

    #[error(transparent)]
    JsonError(#[from] serde_json::Error),

    #[error("Could not reply to client")]
    ReplyError,
}

pub type Result<T> = std::result::Result<T, Error>;

// TODO remove serialize
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase", tag = "type")]
enum Request {
    Experiment(experiment::Action),
    Emergency,  
    Drone {
        action: drone::Action,
        uuid: uuid::Uuid
    },
    PiPuck {
        action: pipuck::Action,
        uuid: uuid::Uuid
    },
    Update {
        tab: String
    },
    Firmware {
        action: firmware::Action,
        file: Option<(String, String)>,
        uuid: uuid::Uuid
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "lowercase", tag = "type", content = "action")]
enum Action {
    Drone(drone::Action),
    PiPuck(pipuck::Action),
    Experiment(experiment::Action),
    Firmware(firmware::Action),
}

#[derive(Serialize, Debug)]
struct Card {
    span: u8,
    title: String,
    content: Content,
    actions: Vec<Action>,
}

type Cards = HashMap<uuid::Uuid, Card>;

// TODO, Reply will probably need to be wrapped in a enum soon Reply::Update, Reply::XXX
#[derive(Serialize)]
struct Reply {
    title: String,
    cards: Cards,
}

lazy_static::lazy_static! {
    /* UUIDs */
    static ref UUID_CONFIG: uuid::Uuid =
        uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_OID, "experiment".as_bytes());
    static ref UUID_CONFIG_DRONE: uuid::Uuid =
        uuid::Uuid::new_v3(&UUID_CONFIG, "drones".as_bytes());
    static ref UUID_CONFIG_PIPUCK: uuid::Uuid =
        uuid::Uuid::new_v3(&UUID_CONFIG, "pipucks".as_bytes());
    
    /* other */
    static ref IIO_CHECKS: Vec<(String, String)> =
            ["epuck-groundsensors", "epuck-motors", "epuck-leds", "epuck-rangefinders"].iter()
            .map(|dev| (String::from(*dev), format!("grep ^{} /sys/bus/iio/devices/*/name", dev)))
            .collect::<Vec<_>>();
    static ref REGEX_IIO_DEVICE: Regex = Regex::new(r"iio:device[[:digit:]]+").unwrap();
}

pub async fn run(ws: ws::WebSocket,
                 drones: Robots<drone::Drone>,
                 pipucks: Robots<pipuck::PiPuck>,
                 experiment: Experiment) {
    // Use a counter to assign a new unique ID for this user.

    log::info!("client connected!");
    // Split the socket into a sender and receive of messages.
    let (user_ws_tx, mut user_ws_rx) = ws.split();

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn(rx.forward(user_ws_tx).map(|result| {
        if let Err(error) = result {
            log::error!("websocket send failed: {}", error);
        }
    }));

    // this loop is basically our gui updating thread
    while let Some(data) = user_ws_rx.next().await {
        let request : ws::Message = match data {
            Ok(request) => request,
            Err(error) => {
                log::error!("websocket receive failed: {}", error);
                break;
            }
        };
        if let Ok(request) = request.to_str() {
            /*
            let t1 = Request::Upload{ target: "irobot".to_owned(), filename: "control.lua".to_owned(), data: "4591345879dsfsd908g".to_owned()};
            let t2 = Request::PiPuck{ action: pipuck::Action::RpiReboot, uuid: uuid::Uuid::new_v4()};
            let t3 = Request::Update{ tab: "Connections".to_owned() };
            eprintln!("t1 = {}", serde_json::to_string(&t1).unwrap());
            eprintln!("t2 = {}", serde_json::to_string(&t2).unwrap());
            eprintln!("t3 = {}", serde_json::to_string(&t3).unwrap());
            */
            if let Ok(action) = serde_json::from_str::<Request>(request) {
                match action {
                    Request::Emergency => {
                        // Go to emergency mode
                    },
                    Request::Experiment(action) => {
                        experiment.write().await.execute(&action);
                    },
                    Request::Drone{action, uuid} => {
                        let mut drones = drones.write().await;
                        if let Some(drone) = drones.iter_mut().find(|drone| drone.uuid == uuid) {
                            drone.execute(&action);
                        }
                        else {
                            log::warn!("Could not execute {:?}, drone ({}) has disconnected", action, uuid);
                        }
                    },
                    Request::PiPuck{action, uuid} => {
                        let mut pipucks = pipucks.write().await;
                        if let Some(pipuck) = pipucks.iter_mut().find(|pipuck| pipuck.uuid == uuid) {
                            pipuck.execute(&action).await;
                        }
                        else {
                            log::warn!("Could not execute {:?}, Pi-Puck ({}) has disconnected", action, uuid);
                        }
                    },
                    Request::Update{tab} => {
                        let reply = match &tab[..] {
                            "connections" => Ok(connections_tab(&drones, &pipucks).await),
                            "diagnostics" => Ok(diagnostics_tab(&drones, &pipucks).await),
                            "experiment" => Ok(experiment_tab(&drones, &pipucks, &experiment).await),
                            "optitrack" => Ok(optitrack_tab().await),
                            _ => Err(Error::BadRequest),
                        };
                        let result = reply
                            .and_then(|inner| {
                                serde_json::to_string(&inner).map_err(|err| Error::JsonError(err))
                            }).and_then(|inner| {
                                let message = Ok(ws::Message::text(inner));
                                tx.send(message).map_err(|_| Error::ReplyError)
                            });
                        if let Err(error) = result {
                            log::error!("Could not reply to client: {}", error);
                        }
                    },
                    Request::Firmware{action, uuid, file} => {
                        match action {
                            firmware::Action::Upload => {
                                let file = file.and_then(|(name, content)| {
                                    match content.split(',').tuples::<(_,_)>().next() {
                                        Some((_, data)) => {
                                            match base64::decode(data) {
                                                Ok(data) => Some((name, data)),
                                                Err(error) => {
                                                    log::error!("Could not decode {}: {}", name, error);
                                                    None
                                                }
                                            }
                                        },
                                        None => None
                                    }
                                });
                                if let Some((filename, content)) = &file {
                                    if uuid == *UUID_CONFIG_DRONE {
                                        let mut drones = drones.write().await;
                                        let mut tasks = drones
                                            .iter_mut()
                                            .filter_map(|drone| drone.ssh())
                                            .map(|drone| {
                                                drone.add_ctrl_software(filename, content)
                                            })
                                            .collect::<FuturesUnordered<_>>();
                                        while let Some(result) = tasks.next().await {
                                            if let Err(error) = result {
                                                log::error!("Failed to add control software {}: {}", filename, error)
                                            }
                                        }
                                    }
                                    else if uuid == *UUID_CONFIG_PIPUCK {
                                        let mut pipucks = pipucks.write().await;
                                        let mut tasks = pipucks
                                            .iter_mut()
                                            .map(|pipuck| pipuck.ssh.add_ctrl_software(filename, content))
                                            .collect::<FuturesUnordered<_>>();
                                        while let Some(result) = tasks.next().await {
                                            if let Err(error) = result {
                                                log::error!("Failed to add control software {}: {}", filename, error)
                                            }
                                        }
                                    }
                                    else {
                                        log::error!("UUID target does not support adding control software");
                                    }
                                }
                            }
                            firmware::Action::Clear => {
                                if uuid == *UUID_CONFIG_DRONE {
                                    let mut drones = drones.write().await;
                                    let mut tasks = drones
                                        .iter_mut()
                                        .filter_map(|drone| drone.ssh())
                                        .map(|drone| {
                                            drone.clear_ctrl_software()
                                        })
                                        .collect::<FuturesUnordered<_>>();
                                    while let Some(result) = tasks.next().await {
                                        if let Err(error) = result {
                                            log::error!("Failed to clear control software: {}", error);
                                        }
                                    }
                                }
                                else if uuid == *UUID_CONFIG_PIPUCK {
                                    let mut pipucks = pipucks.write().await;
                                    let mut tasks = pipucks
                                        .iter_mut()
                                        .map(|pipuck| pipuck.ssh.clear_ctrl_software())
                                        .collect::<FuturesUnordered<_>>();
                                    while let Some(result) = tasks.next().await {
                                        if let Err(error) = result {
                                            log::error!("Failed to clear control software: {}", error);
                                        }
                                    }
                                }
                                else {
                                    log::error!("UUID target does not support clearing control software");
                                }
                            }
                        }
                    },
                }
            }
            else {
                log::error!("cannot not deserialize message");
            }
        }
    }
    log::info!("client disconnected!");
}

async fn diagnostics_tab(_drones: &Robots<drone::Drone>, pipucks: &Robots<pipuck::PiPuck>) -> Reply {
    let mut pipucks = pipucks.write().await;
    let mut tasks = pipucks
        .iter_mut()
        .map(|pipuck| async move {
            (pipuck.uuid.clone(), pipuck.ssh.ctrl_software().await)
        })
        .collect::<FuturesUnordered<_>>();
    /* hashmap of cards */
    let mut cards = Cards::default();
    /* create a card for each robot once it replies */
    while let Some((uuid, result)) = tasks.next().await {
        let card = match result {
            Ok(files) => {
                /* table header */
                let header = ["File", "Checksum"].iter().map(|s| {
                    String::from(*s)
                }).collect::<Vec<_>>();
                /* card */
                Card {
                    span: 3,
                    title: format!("Pi-Puck"),
                    content: Content::Table {
                        header: header,
                        rows: files
                            .into_iter()
                            .map(|(checksum, path)| vec![path, checksum])
                            .collect()
                    },
                    actions: Vec::default(),
                }
            },
            Err(error) => {
                let error = format!("{} {}", ERROR_ICON, error);
                Card {
                    span: 3,
                    title: format!("Pi-Puck"),
                    content: Content::Text(error),
                    actions: Vec::default(),
                }
            }
        };
        cards.insert(uuid, card);
    }
    Reply { title: "Diagnostics".to_owned(), cards }
}

async fn experiment_tab(_: &Robots<drone::Drone>, _: &Robots<pipuck::PiPuck>, experiment: &Experiment) -> Reply {
    let mut cards = Cards::default();
    let card = Card {
        span: 6,
        title: String::from("Drone Configuration"),
        content: Content::Text(String::from("Drone")),
        // the actions depend on the state of the drone
        // the action part of the message must contain
        // the uuid, action name, and optionally arguments
        actions: vec![firmware::Action::Upload, firmware::Action::Clear]
            .into_iter().map(Action::Firmware).collect(),
    };
    cards.insert(*UUID_CONFIG_DRONE, card);
    let card = Card {
        span: 6,
        title: String::from("Pi-Puck Configuration"),
        content: Content::Text(String::from("Drone")),
        // the actions depend on the state of the drone
        // the action part of the message must contain
        // the uuid, action name, and optionally arguments
        actions: vec![firmware::Action::Upload, firmware::Action::Clear]
            .into_iter().map(Action::Firmware).collect(),
    };
    cards.insert(*UUID_CONFIG_PIPUCK, card);
    let card = Card {
        span: 12,
        title: String::from("Dashboard"),
        content: Content::Text(String::from("Drone")),
        // the actions depend on the state of the drone
        // the action part of the message must contain
        // the uuid, action name, and optionally arguments
        actions: experiment.read().await.actions().into_iter().map(Action::Experiment).collect(), // start/stop experiment
    };
    cards.insert(uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_OID, "experiment:dashboard".as_bytes()), card);
    Reply { title: "Experiment".to_owned(), cards }
}

async fn optitrack_tab() -> Reply {
    let mut cards = Cards::default();

    if let Ok(inner) = timeout(Duration::from_millis(100), optitrack::once()).await {
        if let Ok(frame_of_data) = inner {
            for rigid_body in frame_of_data.rigid_bodies {
                let position = format!("x = {:.3}, y = {:.3}, z = {:.3}",
                    rigid_body.position.x,
                    rigid_body.position.y,
                    rigid_body.position.z);
                let orientation = format!("w = {:.3}, x = {:.3}, y = {:.3}, z = {:.3}",
                    rigid_body.orientation.w,
                    rigid_body.orientation.vector().x,
                    rigid_body.orientation.vector().y,
                    rigid_body.orientation.vector().z);
                let card = Card {
                    span: 3,
                    title: format!("Rigid body {}", rigid_body.id),
                    content: Content::Table {
                        header: vec!["Position".to_owned(), "Orientation".to_owned()],
                        rows: vec![vec![position, orientation]]
                    },
                    // the actions depend on the state of the drone
                    // the action part of the message must contain
                    // the uuid, action name, and optionally arguments
                    actions: vec![], // start/stop experiment
                };
                cards.insert(uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_OID, &rigid_body.id.to_be_bytes()), card);
            }
        }
        Reply { title: "Optitrack".to_owned(), cards }
    }
    else {
        Reply { title: "Optitrack [OFFLINE]".to_owned(), cards }
    }
}

async fn connections_tab(drones: &Robots<drone::Drone>, pipucks: &Robots<pipuck::PiPuck>) -> Reply {
    let mut cards = Cards::default();
    for drone in drones.read().await.iter() {
        let card = Card {
            span: 4,
            title: String::from("Drone"),
            content: Content::Table {
                header: vec!["Unique Identifier".to_owned(), "Xbee Address".to_owned(), "SSH Address".to_owned()],
                rows: vec![vec![drone.uuid.to_string(), drone.xbee.addr.to_string(), String::from("-")]]
            },
            actions: drone.actions().into_iter().map(Action::Drone).collect(),
        };
        cards.insert(drone.uuid.clone(), card);
    }
    for pipuck in pipucks.read().await.iter() {
        let card = Card {
            span: 4,
            title: String::from("Pi-Puck"),
            content: Content::Table {
                header: vec!["Unique Identifier".to_owned(), "SSH Address".to_owned()],
                rows: vec![vec![pipuck.uuid.to_string(), pipuck.ssh.addr.to_string()]]
            },
            actions: pipuck.actions().into_iter().map(Action::PiPuck).collect(),
        };
        cards.insert(pipuck.uuid.clone(), card);
    }
    Reply { title: "Connections".to_owned(), cards }
}